//! Vault: line-level content-addressed immutable backup of raw sessions.
//!
//! Why line-level: session JSONL files grow by appends, so whole-file
//! hashing never dedups. Individual lines are immutable once written, which
//! makes SHA-256(line) a perfect content address: every line of every
//! session is stored exactly once, and a session file can be rebuilt from
//! its manifest (the `vault_lines` table) at any time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::{Read, Write}};

const COMPRESSED_MAGIC: &[u8; 8] = b"YMEMGZ1\0";
const COMPRESS_MIN_BYTES: usize = 256;

pub fn objects_root(home: &Path) -> PathBuf {
    home.join("objects")
}

pub fn object_path(home: &Path, hash: &str) -> PathBuf {
    // 防御式（codex 评审 blocker）：异常短哈希不再 panic——get 取不到两位分片
    // 时整串当分片名（非法哈希本就不该有对象，读取会干净地 not found）
    let shard = hash.get(..2).unwrap_or(hash);
    objects_root(home).join(shard).join(hash)
}

/// Store one raw line (without the trailing newline). Returns its hash.
pub fn store_line(home: &Path, bytes: &[u8]) -> Result<String> {
    let hash = hex_sha256(bytes);
    let path = object_path(home, &hash);
    if path.exists() {
        return Ok(hash);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Write-then-rename so a crash never leaves a truncated object. The tmp
    // name carries the pid: two concurrent yourmem processes storing the same
    // content would otherwise share one temp file and could rename over each
    // other mid-write (the object itself is immutable, the race is only on tmp).
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, encode_object(bytes)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(hash)
}

/// Semantic alias for `store_line` (DESIGN-0.3 §2): native memory files are
/// archived as whole-file bytes — they are rewritten in place, unlike the
/// append-only session JSONL, so line-level addressing doesn't apply.
pub fn store_bytes(home: &Path, bytes: &[u8]) -> Result<String> {
    store_line(home, bytes)
}

/// Read a vault object. With `verify`, re-hash the bytes and compare against
/// the content address — this is the byte-level fidelity proof behind
/// `backup export` ("恢复出来的文件与原件逐字节一致" must be checked, not assumed).
pub fn read_object(home: &Path, hash: &str, verify: bool) -> Result<Vec<u8>> {
    let path = object_path(home, hash);
    let stored = std::fs::read(&path).with_context(|| format!("missing vault object {hash}"))?;
    let bytes = decode_object(&stored).with_context(|| format!("decode vault object {hash}"))?;
    if verify {
        anyhow::ensure!(
            hex_sha256(&bytes) == hash,
            "vault object {hash} failed hash verification (corrupted on disk)"
        );
    }
    Ok(bytes)
}

fn encode_object(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < COMPRESS_MIN_BYTES { return Ok(bytes.to_vec()); }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes)?;
    let compressed = encoder.finish()?;
    if compressed.len() + COMPRESSED_MAGIC.len() >= bytes.len() { return Ok(bytes.to_vec()); }
    let mut stored = Vec::with_capacity(COMPRESSED_MAGIC.len() + compressed.len());
    stored.extend_from_slice(COMPRESSED_MAGIC);
    stored.extend_from_slice(&compressed);
    Ok(stored)
}

fn decode_object(stored: &[u8]) -> Result<Vec<u8>> {
    if !stored.starts_with(COMPRESSED_MAGIC) { return Ok(stored.to_vec()); }
    let mut decoded = Vec::new();
    GzDecoder::new(&stored[COMPRESSED_MAGIC.len()..]).read_to_end(&mut decoded)?;
    Ok(decoded)
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex_sha256(bytes)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Rebuild the raw session JSONL from the vault, verifying every object's
/// hash as it is read. Proves the backup is real.
pub fn export_session(conn: &Connection, home: &Path, session_id: &str, out: &Path) -> Result<u64> {
    let mut stmt = conn.prepare(
        "SELECT hash FROM vault_lines WHERE session_id = ?1 ORDER BY line_no",
    )?;
    let hashes: Vec<String> = stmt
        .query_map(rusqlite::params![session_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    anyhow::ensure!(!hashes.is_empty(), "no vault manifest for session {session_id}");

    let mut buf = Vec::new();
    for h in &hashes {
        buf.extend_from_slice(&read_object(home, h, true)?);
        buf.push(b'\n');
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, &buf)?;
    Ok(hashes.len() as u64)
}

// ------------------------------------------------------ proof card (UI §3)

/// 资产证明卡数据（UI-DESIGN §3）：单个会话的 vault 归档清单统计——行数、
/// 去重后对象数、实测字节数、独占对象数、磁盘缺失数（诚实上报：对象丢了
/// 就说丢了，证明卡的意义恰恰是能发现这件事）。
pub fn session_proof(conn: &Connection, home: &Path, session_id: &str) -> Result<Value> {
    let (lines, distinct): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT hash) FROM vault_lines WHERE session_id = ?1",
        rusqlite::params![session_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    anyhow::ensure!(lines > 0, "会话 {session_id} 没有 vault 归档");
    let exclusive: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (
            SELECT hash FROM vault_lines WHERE session_id = ?1
            EXCEPT SELECT hash FROM vault_lines WHERE session_id != ?1
            EXCEPT SELECT hash FROM memory_revisions)",
        rusqlite::params![session_id],
        |r| r.get(0),
    )?;
    let hashes: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT hash FROM vault_lines WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut bytes = 0u64;
    let mut missing = 0u64;
    for h in &hashes {
        match std::fs::metadata(object_path(home, h)) {
            Ok(m) => bytes += m.len(),
            Err(_) => missing += 1,
        }
    }
    Ok(json!({
        "lines": lines,
        "distinct_objects": distinct,
        "exclusive_objects": exclusive,
        "objects_bytes": bytes,
        "objects_missing": missing,
    }))
}

/// 立即校验（UI-DESIGN §3 招牌交互）：逐对象重算哈希对账内容地址。
/// 与 export_session 的校验同一纪律——保真是"验"出来的，不是假设的。
pub fn verify_session(conn: &Connection, home: &Path, session_id: &str) -> Result<Value> {
    let hashes: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT hash FROM vault_lines WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    anyhow::ensure!(!hashes.is_empty(), "会话 {session_id} 没有 vault 归档");
    let mut verified = 0u64;
    let mut failed: Vec<String> = Vec::new();
    for h in &hashes {
        match read_object(home, h, true) {
            Ok(_) => verified += 1,
            Err(_) => failed.push(h.clone()),
        }
    }
    Ok(json!({
        "ok": failed.is_empty(),
        "objects": hashes.len(),
        "verified": verified,
        "failed": failed,
        "checked_at": crate::now_iso(),
    }))
}

pub fn status(conn: &Connection, home: &Path) -> Result<Value> {
    let lines: i64 = conn.query_row("SELECT COUNT(*) FROM vault_lines", [], |r| r.get(0))?;
    let distinct: i64 = conn.query_row("SELECT COUNT(DISTINCT hash) FROM vault_lines", [], |r| r.get(0))?;
    let sessions: i64 = conn.query_row("SELECT COUNT(DISTINCT session_id) FROM vault_lines", [], |r| r.get(0))?;

    let mut objects = 0u64;
    let mut bytes = 0u64;
    let mut tmp_residue = 0u64;
    let root = objects_root(home);
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                // store_line 崩溃残留的 .tmp.<pid> 不是对象，别计入对象数
                //（自检 B1）——计入会造成"对象数 > distinct 引用数"的假象
                if entry.file_name().to_string_lossy().contains(".tmp.") {
                    tmp_residue += 1;
                    continue;
                }
                objects += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    Ok(json!({
        "sessions_archived": sessions,
        "lines_archived": lines,
        "distinct_line_objects": distinct,
        "objects_on_disk": objects,
        "objects_bytes": bytes,
        "tmp_residue": tmp_residue,
        "dedup_ratio": if lines > 0 { (distinct as f64 / lines as f64 * 100.0).round() / 100.0 } else { 1.0 },
        "db_snapshots": list_snapshots(home)?.len(),
    }))
}

fn orphan_inventory(conn: &Connection, home: &Path) -> Result<Vec<(String, u64)>> {
    let mut refs = HashSet::new();
    for sql in ["SELECT DISTINCT hash FROM vault_lines", "SELECT DISTINCT hash FROM memory_revisions"] {
        let mut stmt = conn.prepare(sql)?;
        refs.extend(stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?);
    }
    let mut rows = Vec::new();
    let root = objects_root(home);
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() { continue; }
            let name = entry.file_name().to_string_lossy().to_string();
            let valid_hash = name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
            if name.contains(".tmp.") || (valid_hash && !refs.contains(&name)) {
                rows.push((entry.path().to_string_lossy().to_string(), entry.metadata().map(|m| m.len()).unwrap_or(0)));
            }
        }
    }
    rows.sort();
    Ok(rows)
}

pub fn orphan_cleanup_plan(conn: &Connection, home: &Path) -> Result<Value> {
    let rows = orphan_inventory(conn, home)?;
    Ok(json!({"token":hash_bytes(&serde_json::to_vec(&rows)?),"files":rows.len(),
        "bytes":rows.iter().map(|x|x.1).sum::<u64>()}))
}

pub fn orphan_cleanup(conn: &Connection, home: &Path, token: &str) -> Result<Value> {
    let _lock = crate::ingest::ImportLockTx::acquire(home, std::time::Duration::from_secs(120))?;
    let rows = orphan_inventory(conn, home)?;
    anyhow::ensure!(hash_bytes(&serde_json::to_vec(&rows)?) == token, "对象库已变化，请重新预览");
    let mut removed = 0u64;
    let mut reclaimed = 0u64;
    for (raw, size) in rows {
        let path = PathBuf::from(&raw);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let referenced: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM vault_lines WHERE hash=?1) OR EXISTS(SELECT 1 FROM memory_revisions WHERE hash=?1)",
            rusqlite::params![name], |r| r.get(0))?;
        if referenced == 0 && path.is_file() {
            std::fs::remove_file(&path)?;
            removed += 1;
            reclaimed += size;
        }
    }
    Ok(json!({"removed":removed,"reclaimed_bytes":reclaimed}))
}

// ------------------------------------------------------------ db snapshots

fn snapshots_dir(home: &Path) -> PathBuf {
    crate::backups_dir(home).join("db")
}

pub fn list_snapshots(home: &Path) -> Result<Vec<PathBuf>> {
    let dir = snapshots_dir(home);
    let mut out: Vec<PathBuf> = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().map(|e| e == "sqlite").unwrap_or(false) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Consistent DB snapshot via VACUUM INTO (safe while the DB is in use),
/// then prune old snapshots beyond `keep`.
pub fn snapshot_db(conn: &Connection, home: &Path, keep: usize) -> Result<Value> {
    let dir = snapshots_dir(home);
    std::fs::create_dir_all(&dir)?;
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f");
    let path = dir.join(format!("yourmem-{ts}.sqlite"));
    conn.execute("VACUUM INTO ?1", rusqlite::params![path.to_string_lossy().as_ref()])?;

    let snapshots = list_snapshots(home)?;
    let mut pruned = 0u64;
    if snapshots.len() > keep {
        for old in &snapshots[..snapshots.len() - keep] {
            std::fs::remove_file(old)?;
            pruned += 1;
        }
    }
    Ok(json!({
        "snapshot": path,
        "snapshots_kept": snapshots.len() - pruned as usize,
        "pruned": pruned,
    }))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE vault_lines (session_id TEXT, line_no INTEGER, hash TEXT,
             PRIMARY KEY (session_id, line_no));
             CREATE TABLE memory_revisions (id INTEGER PRIMARY KEY, hash TEXT);",
        )
        .unwrap();
        (dir, conn)
    }

    fn archive(conn: &Connection, home: &Path, sid: &str, lines: &[&str]) -> Vec<String> {
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let h = store_line(home, l.as_bytes()).unwrap();
                conn.execute(
                    "INSERT INTO vault_lines (session_id, line_no, hash) VALUES (?1, ?2, ?3)",
                    rusqlite::params![sid, i as i64 + 1, h],
                )
                .unwrap();
                h
            })
            .collect()
    }

    #[test]
    fn proof_counts_and_bytes() {
        let (dir, conn) = setup();
        // "shared" 被两个会话引用：proof 里计入 distinct/bytes，不计入 exclusive
        archive(&conn, dir.path(), "a:x", &["l1", "shared"]);
        archive(&conn, dir.path(), "a:y", &["shared", "l3"]);
        let p = session_proof(&conn, dir.path(), "a:x").unwrap();
        assert_eq!(p["lines"], 2);
        assert_eq!(p["distinct_objects"], 2);
        assert_eq!(p["exclusive_objects"], 1);
        assert_eq!(p["objects_bytes"], 2 + 6); // "l1" + "shared"
        assert_eq!(p["objects_missing"], 0);
    }

    #[test]
    fn verify_detects_corruption_and_loss() {
        let (dir, conn) = setup();
        let hashes = archive(&conn, dir.path(), "a:x", &["l1", "l2", "l3"]);
        let v = verify_session(&conn, dir.path(), "a:x").unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["verified"], 3);

        // 篡改一个字节级内容（直接覆写对象文件）→ 校验必须抓到
        std::fs::write(object_path(dir.path(), &hashes[0]), b"tampered").unwrap();
        let v = verify_session(&conn, dir.path(), "a:x").unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["failed"].as_array().unwrap().len(), 1);

        // 对象丢失 → proof 如实上报 missing，校验同样抓到
        std::fs::remove_file(object_path(dir.path(), &hashes[1])).unwrap();
        let p = session_proof(&conn, dir.path(), "a:x").unwrap();
        assert_eq!(p["objects_missing"], 1);
        let v = verify_session(&conn, dir.path(), "a:x").unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["failed"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn compresses_large_objects_and_reads_legacy_objects() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "repeated tool output ".repeat(20_000).into_bytes();
        let hash = store_line(dir.path(), &raw).unwrap();
        let path = object_path(dir.path(), &hash);
        assert!(path.metadata().unwrap().len() < raw.len() as u64 / 4);
        assert_eq!(read_object(dir.path(), &hash, true).unwrap(), raw);
        let legacy = b"legacy uncompressed object";
        let legacy_hash = hash_bytes(legacy);
        let legacy_path = object_path(dir.path(), &legacy_hash);
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, legacy).unwrap();
        assert_eq!(read_object(dir.path(), &legacy_hash, true).unwrap(), legacy);
    }

    #[test]
    fn orphan_cleanup_preserves_referenced_objects() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(dir.path()).unwrap();
        let keep = store_line(dir.path(), b"referenced object").unwrap();
        let remove = store_line(dir.path(), b"unreferenced object").unwrap();
        conn.execute(
            "INSERT INTO vault_lines(session_id,line_no,hash) VALUES('test',1,?1)",
            rusqlite::params![keep],
        ).unwrap();
        let p = orphan_cleanup_plan(&conn, dir.path()).unwrap();
        assert_eq!(p["files"], 1);
        orphan_cleanup(&conn, dir.path(), p["token"].as_str().unwrap()).unwrap();
        assert!(object_path(dir.path(), &keep).is_file());
        assert!(!object_path(dir.path(), &remove).exists());
    }
}
