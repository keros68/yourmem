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
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::{Read, Write}};

const COMPRESSED_MAGIC: &[u8; 8] = b"YMEMGZ1\0";
const COMPRESS_MIN_BYTES: usize = 256;

pub fn objects_root(home: &Path) -> PathBuf {
    home.join("objects")
}

/// File layout of one object: `objects/<first two hex>/<hash>`. Bundles use it,
/// and earlier versions stored every object this way; `Store` still reads such
/// files until `migrate_legacy` moves them in.
pub fn object_path(home: &Path, hash: &str) -> PathBuf {
    // 防御式（codex 评审 blocker）：异常短哈希不再 panic——get 取不到两位分片
    // 时整串当分片名（非法哈希本就不该有对象，读取会干净地 not found）
    let shard = hash.get(..2).unwrap_or(hash);
    objects_root(home).join(shard).join(hash)
}

pub fn store_db_path(home: &Path) -> PathBuf {
    home.join("objects.db")
}

/// All vault objects in one SQLite file: a line per file wasted most of the
/// disk on cluster padding and made copying the archive take hours.
/// Commits are fully synced, so an object is durable before the manifest row
/// that references it is committed in the main database.
pub struct Store {
    conn: Connection,
    home: PathBuf,
}

impl Store {
    pub fn open(home: &Path) -> Result<Store> {
        Self::open_at(home, &store_db_path(home))
    }

    /// A store in another file (snapshot repositories); legacy files are
    /// looked up under `home`.
    pub fn open_at(home: &Path, db: &Path) -> Result<Store> {
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=30000; PRAGMA journal_size_limit=67108864;
             CREATE TABLE IF NOT EXISTS objects(hash TEXT PRIMARY KEY, data BLOB NOT NULL);",
        )?;
        Ok(Store { conn, home: home.to_path_buf() })
    }

    fn stored(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT data FROM objects WHERE hash = ?1", [hash], |r| r.get(0))
            .optional()?;
        if row.is_some() {
            return Ok(row);
        }
        match std::fs::read(object_path(&self.home, hash)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn contains(&self, hash: &str) -> Result<bool> {
        let in_db: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
            [hash],
            |r| r.get(0),
        )?;
        Ok(in_db || object_path(&self.home, hash).is_file())
    }

    /// Stored (possibly compressed) size, or None when the object is missing.
    pub fn stored_len(&self, hash: &str) -> Result<Option<u64>> {
        let len: Option<i64> = self
            .conn
            .query_row("SELECT length(data) FROM objects WHERE hash = ?1", [hash], |r| r.get(0))
            .optional()?;
        if let Some(n) = len {
            return Ok(Some(n as u64));
        }
        Ok(std::fs::metadata(object_path(&self.home, hash)).ok().map(|m| m.len()))
    }

    /// Stored bytes as written (compressed form included), for archiving.
    pub fn raw(&self, hash: &str) -> Result<Vec<u8>> {
        self.stored(hash)?.with_context(|| format!("missing vault object {hash}"))
    }

    /// Read an object. With `verify`, re-hash the bytes and compare against
    /// the content address — this is the byte-level fidelity proof behind
    /// `backup export` ("恢复出来的文件与原件逐字节一致" must be checked, not assumed).
    pub fn get(&self, hash: &str, verify: bool) -> Result<Vec<u8>> {
        let stored = self.raw(hash)?;
        if verify {
            return verified(hash, &stored);
        }
        decode_object(&stored).with_context(|| format!("decode vault object {hash}"))
    }

    /// Store several objects in one synced transaction; returns their hashes
    /// in input order.
    pub fn put_many<'a>(&self, items: impl IntoIterator<Item = &'a [u8]>) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut hashes = Vec::new();
        {
            let mut exists = tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)")?;
            let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO objects(hash, data) VALUES (?1, ?2)")?;
            for bytes in items {
                let hash = hex_sha256(bytes);
                let present: bool = exists.query_row([&hash], |r| r.get(0))?;
                if !present && !object_path(&self.home, &hash).is_file() {
                    insert.execute(rusqlite::params![hash, encode_object(bytes)?])?;
                }
                hashes.push(hash);
            }
        }
        tx.commit()?;
        Ok(hashes)
    }

    /// Insert stored-form bytes whose content address the caller has checked.
    /// An existing row that no longer matches its address is replaced.
    pub fn put_raw(&self, hash: &str, stored: &[u8]) -> Result<()> {
        let existing: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT data FROM objects WHERE hash = ?1", [hash], |r| r.get(0))
            .optional()?;
        match existing {
            Some(data) if verified(hash, &data).is_ok() => {}
            Some(_) => {
                self.conn.execute("UPDATE objects SET data = ?2 WHERE hash = ?1", rusqlite::params![hash, stored])?;
            }
            None => {
                self.conn.execute(
                    "INSERT INTO objects(hash, data) VALUES (?1, ?2)",
                    rusqlite::params![hash, stored],
                )?;
            }
        }
        Ok(())
    }

    /// Group writes into one synced transaction.
    pub fn transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    /// Remove an object from the store and any legacy file.
    pub fn remove(&self, hash: &str) -> Result<()> {
        self.conn.execute("DELETE FROM objects WHERE hash = ?1", [hash])?;
        match std::fs::remove_file(object_path(&self.home, hash)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Every stored object with its stored size, legacy files included.
    pub fn inventory(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare("SELECT hash, length(data) FROM objects")?;
        let mut rows: Vec<(String, u64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.extend(legacy_files(&self.home));
        Ok(rows)
    }
}

/// Decode stored-form bytes and check them against their content address.
pub fn verified(hash: &str, stored: &[u8]) -> Result<Vec<u8>> {
    let bytes = decode_object(stored).with_context(|| format!("decode vault object {hash}"))?;
    anyhow::ensure!(
        hex_sha256(&bytes) == hash,
        "vault object {hash} failed hash verification (corrupted on disk)"
    );
    Ok(bytes)
}

/// Disk used by the object store file and any legacy object files.
pub fn store_disk_bytes(home: &Path) -> u64 {
    let db = store_db_path(home);
    let wal = home.join("objects.db-wal");
    let files: u64 = [db, wal].iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    files + legacy_files(home).iter().map(|x| x.1).sum::<u64>()
}

fn is_hash_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Object files written by earlier versions (`objects/<shard>/<hash>`).
fn legacy_files(home: &Path) -> Vec<(String, u64)> {
    let root = objects_root(home);
    if !root.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            is_hash_name(&name).then(|| (name, e.metadata().map(|m| m.len()).unwrap_or(0)))
        })
        .collect()
}

/// Move up to `limit` legacy object files into the store under the import
/// lock. Files are deleted only after the batch is committed; a file whose
/// content no longer matches its address is left in place for the self-check.
pub fn migrate_legacy(home: &Path, limit: usize) -> Result<Value> {
    let root = objects_root(home);
    if !root.is_dir() {
        return Ok(json!({"moved": 0, "skipped": 0, "remaining": false}));
    }
    let _lock = crate::ingest::ImportLockTx::acquire(home, std::time::Duration::from_secs(120))?;
    let store = Store::open(home)?;
    let mut batch = Vec::new();
    let mut remaining = false;
    for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
        let Some(name) = entry.file_name().to_str() else { continue };
        if !entry.file_type().is_file() || !is_hash_name(name) {
            continue;
        }
        if batch.len() == limit {
            remaining = true;
            break;
        }
        batch.push((name.to_string(), entry.path().to_path_buf()));
    }
    let mut moved = Vec::new();
    let mut skipped = 0u64;
    let tx = store.transaction()?;
    for (hash, path) in &batch {
        let stored = std::fs::read(path)?;
        if verified(hash, &stored).is_ok() {
            // put_raw 会替换库里已损坏的同名记录：删旧文件前库内必须是完好副本
            store.put_raw(hash, &stored)?;
            moved.push(path);
        } else {
            skipped += 1;
        }
    }
    tx.commit()?;
    for path in &moved {
        std::fs::remove_file(path)?;
        if let Some(shard) = path.parent() {
            let _ = std::fs::remove_dir(shard); // succeeds only once empty
        }
    }
    if !remaining {
        // Empty shard directories, then the root; both fail harmlessly while
        // anything (a skipped file, a temp leftover) is still inside.
        for entry in std::fs::read_dir(&root)?.flatten() {
            let _ = std::fs::remove_dir(entry.path());
        }
        let _ = std::fs::remove_dir(&root);
    }
    Ok(json!({"moved": moved.len(), "skipped": skipped, "remaining": remaining}))
}

/// Store one raw line (without the trailing newline). Returns its hash.
pub fn store_line(home: &Path, bytes: &[u8]) -> Result<String> {
    Ok(Store::open(home)?.put_many([bytes])?.remove(0))
}

/// Semantic alias for `store_line` (DESIGN-0.3 §2): native memory files are
/// archived as whole-file bytes — they are rewritten in place, unlike the
/// append-only session JSONL, so line-level addressing doesn't apply.
pub fn store_bytes(home: &Path, bytes: &[u8]) -> Result<String> {
    store_line(home, bytes)
}

/// Read one vault object; see `Store::get`. Callers reading many objects
/// should open a `Store` once instead.
pub fn read_object(home: &Path, hash: &str, verify: bool) -> Result<Vec<u8>> {
    Store::open(home)?.get(hash, verify)
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

    let store = Store::open(home)?;
    let mut buf = Vec::new();
    for h in &hashes {
        buf.extend_from_slice(&store.get(h, true)?);
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
    let store = Store::open(home)?;
    let mut bytes = 0u64;
    let mut missing = 0u64;
    for h in &hashes {
        match store.stored_len(h)? {
            Some(n) => bytes += n,
            None => missing += 1,
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
    let store = Store::open(home)?;
    let mut verified = 0u64;
    let mut failed: Vec<String> = Vec::new();
    for h in &hashes {
        match store.get(h, true) {
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

    let inventory = Store::open(home)?.inventory()?;
    let objects = inventory.len() as u64;
    let bytes = store_disk_bytes(home);
    let tmp_residue = tmp_files(home).len() as u64;
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

/// Crash leftovers from earlier versions' file writes (`<hash>.tmp.<pid>`).
pub fn tmp_files(home: &Path) -> Vec<PathBuf> {
    let root = objects_root(home);
    if !root.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name().to_string_lossy().contains(".tmp."))
        .map(|e| e.into_path())
        .collect()
}

/// Unreferenced objects (by hash) and temp leftovers (by path), with sizes.
fn orphan_inventory(conn: &Connection, home: &Path) -> Result<Vec<(String, u64)>> {
    let mut refs = HashSet::new();
    for sql in ["SELECT DISTINCT hash FROM vault_lines", "SELECT DISTINCT hash FROM memory_revisions"] {
        let mut stmt = conn.prepare(sql)?;
        refs.extend(stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?);
    }
    let mut rows: Vec<(String, u64)> = Store::open(home)?
        .inventory()?
        .into_iter()
        .filter(|(hash, _)| !refs.contains(hash))
        .collect();
    for path in tmp_files(home) {
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        rows.push((path.to_string_lossy().to_string(), size));
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
    let store = Store::open(home)?;
    let mut removed = 0u64;
    let mut reclaimed = 0u64;
    for (key, size) in rows {
        if !is_hash_name(&key) {
            std::fs::remove_file(&key)?;
        } else {
            let referenced: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM vault_lines WHERE hash=?1) OR EXISTS(SELECT 1 FROM memory_revisions WHERE hash=?1)",
                rusqlite::params![key], |r| r.get(0))?;
            if referenced != 0 {
                continue;
            }
            store.remove(&key)?;
        }
        removed += 1;
        reclaimed += size;
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

        // 篡改一个字节级内容（直接改写对象库）→ 校验必须抓到
        let objects = Connection::open(store_db_path(dir.path())).unwrap();
        objects.execute("UPDATE objects SET data = x'00' WHERE hash = ?1", [&hashes[0]]).unwrap();
        let v = verify_session(&conn, dir.path(), "a:x").unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["failed"].as_array().unwrap().len(), 1);

        // 对象丢失 → proof 如实上报 missing，校验同样抓到
        objects.execute("DELETE FROM objects WHERE hash = ?1", [&hashes[1]]).unwrap();
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
        let stored = Store::open(dir.path()).unwrap().stored_len(&hash).unwrap().unwrap();
        assert!(stored < raw.len() as u64 / 4);
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
        let store = Store::open(dir.path()).unwrap();
        assert!(store.contains(&keep).unwrap());
        assert!(!store.contains(&remove).unwrap());
    }

    #[test]
    fn migrate_legacy_moves_intact_files_and_keeps_damaged_ones() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let write_legacy = |bytes: &[u8]| {
            let hash = hash_bytes(bytes);
            let path = object_path(home, &hash);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, encode_object(bytes).unwrap()).unwrap();
            (hash, path)
        };
        let (a, a_path) = write_legacy(b"first legacy line");
        let (b, _) = write_legacy(&"large legacy line ".repeat(100).into_bytes());
        let (bad, bad_path) = write_legacy(b"damaged later");
        std::fs::write(&bad_path, b"not the original").unwrap();

        let first = migrate_legacy(home, 1).unwrap();
        assert_eq!(first["moved"].as_u64().unwrap() + first["skipped"].as_u64().unwrap(), 1);
        assert_eq!(first["remaining"], true);
        let rest = migrate_legacy(home, 100).unwrap();
        assert_eq!(rest["remaining"], false);

        let store = Store::open(home).unwrap();
        assert_eq!(store.get(&a, true).unwrap(), b"first legacy line");
        assert!(store.get(&b, true).is_ok());
        assert!(!a_path.exists(), "moved files are removed");
        assert!(bad_path.exists(), "a damaged file stays for the self-check");
        assert!(store.get(&bad, true).is_err());
    }

    #[test]
    fn migration_replaces_a_corrupt_store_row_instead_of_dropping_the_good_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let bytes = b"only intact copy lives in the legacy file";
        let hash = hash_bytes(bytes);
        let path = object_path(home, &hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, encode_object(bytes).unwrap()).unwrap();
        Store::open(home).unwrap();
        Connection::open(store_db_path(home)).unwrap()
            .execute("INSERT INTO objects(hash, data) VALUES (?1, x'00')", [&hash]).unwrap();
        migrate_legacy(home, 10).unwrap();
        assert_eq!(Store::open(home).unwrap().get(&hash, true).unwrap(), bytes);
    }
}
