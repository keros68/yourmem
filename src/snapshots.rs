//! Local snapshot repository. Immutable compressed blobs are shared across dates;
//! manifests are published last and cleanup is serialized with create/export.
use crate::{bundle, db, ingest::ImportLockTx, vault};
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    id: String,
    created_at: String,
    schema_version: i32,
    db_hash: String,
    db_bytes: u64,
    objects_bytes: u64,
    hashes: BTreeSet<String>,
}

pub fn root(home: &Path) -> PathBuf {
    crate::backups_dir(home).join("snapshots-v1")
}
fn hash_ok(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() < 100 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn regular(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "备份文件不是普通文件"
    );
    Ok(())
}
fn safe_dirs(repo: &Path) -> Result<()> {
    for p in [
        repo.to_path_buf(),
        repo.join("blobs"),
        repo.join("snapshots"),
    ] {
        if p.exists() {
            let m = std::fs::symlink_metadata(&p)?;
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "备份仓库目录不能是链接"
            );
        }
    }
    Ok(())
}
fn lock(repo: &Path) -> Result<ImportLockTx> {
    safe_dirs(repo)?;
    std::fs::create_dir_all(repo.join("blobs"))?;
    std::fs::create_dir_all(repo.join("snapshots"))?;
    let lock = ImportLockTx::acquire(repo, std::time::Duration::from_secs(120))?;
    let marker = repo.join("repository.json");
    if marker.exists() {
        regular(&marker)?;
        let value: Value = serde_json::from_slice(&std::fs::read(marker)?)?;
        ensure!(
            value == json!({"format":"yourmem-snapshots","version":1}),
            "备份仓库格式不受支持"
        );
    } else {
        ensure!(
            std::fs::read_dir(repo.join("blobs"))?.next().is_none()
                && std::fs::read_dir(repo.join("snapshots"))?.next().is_none(),
            "目录已有内容但缺少仓库标记，已停止操作"
        );
        let mut pending = tempfile::NamedTempFile::new_in(repo)?;
        serde_json::to_writer(
            pending.as_file_mut(),
            &json!({"format":"yourmem-snapshots","version":1}),
        )?;
        pending.as_file().sync_all()?;
        pending.persist_noclobber(marker).map_err(|e| e.error)?;
    }
    Ok(lock)
}
fn blob(repo: &Path, hash: &str) -> PathBuf {
    repo.join("blobs").join(format!("{hash}.gz"))
}
fn hash_reader(mut reader: impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn decode(repo: &Path, hash: &str, out: &Path) -> Result<()> {
    ensure!(hash_ok(hash), "备份对象地址无效");
    let path = blob(repo, hash);
    regular(&path)?;
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path)?);
    let mut file = std::fs::File::create(out)?;
    std::io::copy(&mut gz, &mut file)?;
    drop(file);
    ensure!(
        hash_reader(std::fs::File::open(out)?)? == hash,
        "备份对象校验失败"
    );
    Ok(())
}
fn put(repo: &Path, source: &Path, expected: Option<&str>) -> Result<(String, u64)> {
    let hash = hash_reader(std::fs::File::open(source)?)?;
    if let Some(expected) = expected {
        ensure!(hash == expected, "原件校验失败");
    }
    let dst = blob(repo, &hash);
    if dst.exists() {
        regular(&dst)?;
        ensure!(
            hash_reader(flate2::read::GzDecoder::new(std::fs::File::open(&dst)?))? == hash,
            "已有备份对象损坏，请先检查备份仓库"
        );
        return Ok((hash, 0));
    }
    let mut tmp = tempfile::NamedTempFile::new_in(repo.join("blobs"))?;
    let mut encoder =
        flate2::write::GzEncoder::new(tmp.as_file_mut(), flate2::Compression::default());
    std::io::copy(&mut std::fs::File::open(source)?, &mut encoder)?;
    encoder.finish()?;
    tmp.as_file().sync_all()?;
    ensure!(
        hash_reader(flate2::read::GzDecoder::new(std::fs::File::open(
            tmp.path()
        )?))?
            == hash,
        "备份期间原件发生变化，请重试"
    );
    let size = tmp.as_file().metadata()?.len();
    tmp.persist_noclobber(dst).map_err(|e| e.error)?;
    Ok((hash, size))
}
fn manifests(repo: &Path) -> Result<Vec<Snapshot>> {
    safe_dirs(repo)?;
    let dir = repo.join("snapshots");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        regular(&path)?;
        let s: Snapshot = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure!(
            s.version == 1
                && id_ok(&s.id)
                && path.file_stem().and_then(|x| x.to_str()) == Some(&s.id),
            "快照清单无效"
        );
        ensure!(
            hash_ok(&s.db_hash) && s.hashes.iter().all(|h| hash_ok(h)),
            "快照对象地址无效"
        );
        chrono::DateTime::parse_from_rfc3339(&s.created_at)?;
        result.push(s);
    }
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(result)
}
fn inventory(repo: &Path) -> Result<BTreeMap<String, u64>> {
    let mut files = BTreeMap::new();
    let dir = repo.join("blobs");
    if !dir.exists() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("gz") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|x| x.to_str())
            .context("备份文件名无效")?
            .to_string();
        ensure!(hash_ok(&name), "备份文件名无效");
        regular(&path)?;
        files.insert(name, path.metadata()?.len());
    }
    Ok(files)
}
pub fn list(home: &Path) -> Result<Value> {
    let repo = root(home);
    if !repo.exists() {
        return Ok(
            json!({"root":repo,"repository_bytes":0,"snapshots":[],"policy":{"keep_recent":7,"keep_monthly":6}}),
        );
    }
    let _lock = lock(&repo)?;
    let rows = manifests(&repo)?;
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(&repo) {
        let entry = entry?;
        ensure!(!entry.file_type().is_symlink(), "备份仓库不能含链接");
        if entry.file_type().is_file() {
            bytes += entry.metadata()?.len();
        }
    }
    let policy = if repo.join("policy.json").exists() {
        serde_json::from_slice::<Value>(&std::fs::read(repo.join("policy.json"))?)?
    } else {
        json!({"keep_recent":7,"keep_monthly":6})
    };
    Ok(
        json!({"root":repo,"repository_bytes":bytes,"policy":policy,"snapshots":rows.iter().map(|s|json!({
        "id":s.id,"created_at":s.created_at,"db_bytes":s.db_bytes,"objects_bytes":s.objects_bytes,"objects":s.hashes.len()
    })).collect::<Vec<_>>()}),
    )
}
pub fn create(home: &Path) -> Result<Value> {
    let repo = root(home);
    let _repo_lock = lock(&repo)?;
    let _import_lock = ImportLockTx::acquire(home, std::time::Duration::from_secs(120))?;
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("snapshot.sqlite");
    let conn = db::open(home)?;
    conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
    bundle::strip_indexes(&path)?;
    let snap = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let hashes: BTreeSet<_> = bundle::referenced_hashes(&snap)?.into_iter().collect();
    let schema_version = snap.pragma_query_value(None, "user_version", |r| r.get(0))?;
    drop(snap);
    let mut new_objects = 0;
    let mut new_bytes = 0;
    let mut objects_bytes = 0;
    for hash in &hashes {
        ensure!(hash_ok(hash), "原件地址无效");
        let path = vault::object_path(home, hash);
        objects_bytes += path.metadata()?.len();
        let (_, added) = put(&repo, &path, Some(hash))?;
        if added > 0 {
            new_objects += 1;
        }
        new_bytes += added;
    }
    let (db_hash, added) = put(&repo, &path, None)?;
    new_bytes += added;
    let s = Snapshot {
        version: 1,
        id: format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            uuid::Uuid::new_v4()
        ),
        created_at: crate::now_iso(),
        schema_version,
        db_bytes: blob(&repo, &db_hash).metadata()?.len(),
        db_hash,
        objects_bytes,
        hashes,
    };
    inspect_db(&repo, &s, &tmp.path().join("verify.sqlite"))?;
    let mut pending = tempfile::NamedTempFile::new_in(repo.join("snapshots"))?;
    serde_json::to_writer(pending.as_file_mut(), &s)?;
    pending.as_file().sync_all()?;
    new_bytes += pending.as_file().metadata()?.len();
    pending
        .persist_noclobber(repo.join("snapshots").join(format!("{}.json", s.id)))
        .map_err(|e| e.error)?;
    Ok(json!({"id":s.id,"new_objects":new_objects,"new_bytes":new_bytes}))
}
fn inspect_db(repo: &Path, s: &Snapshot, tmp: &Path) -> Result<()> {
    decode(repo, &s.db_hash, tmp)?;
    let conn = Connection::open_with_flags(tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    ensure!(
        s.schema_version <= db::SCHEMA_VERSION,
        "快照来自更新版本，请先升级 yourmem"
    );
    ensure!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))? == "ok",
        "快照数据库损坏"
    );
    ensure!(
        conn.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "快照数据库存在外键错误"
    );
    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_files f WHERE NOT EXISTS
        (SELECT 1 FROM memory_revisions r WHERE r.file_id=f.id AND r.hash=f.current_hash)",
        [],
        |r| r.get(0),
    )?;
    ensure!(missing == 0, "记忆文件缺少当前修订，已停止操作");
    let refs: BTreeSet<_> = bundle::referenced_hashes(&conn)?.into_iter().collect();
    ensure!(refs == s.hashes, "快照清单与数据库引用不一致，已停止操作");
    ensure!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))? == s.schema_version,
        "快照版本不一致"
    );
    Ok(())
}
pub fn export(home: &Path, id: &str, out: &Path) -> Result<Value> {
    ensure!(id_ok(id), "快照 ID 无效");
    let repo = root(home);
    let _lock = lock(&repo)?;
    // An export must never replace a blob, manifest or the live database.
    let parent = out.parent().context("导出路径需要父目录")?;
    std::fs::create_dir_all(parent)?;
    let abs_parent = parent.canonicalize()?;
    ensure!(
        !abs_parent.starts_with(repo.canonicalize()?),
        "请将完整备份导出到快照仓库以外"
    );
    ensure!(
        out.extension().and_then(|x| x.to_str()) == Some("gz"),
        "完整备份需要 .tar.gz 路径"
    );
    let rows = manifests(&repo)?;
    let s = rows.iter().find(|s| s.id == id).context("快照不存在")?;
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("snapshot");
    std::fs::create_dir_all(root.join("db"))?;
    inspect_db(&repo, s, &root.join("db/snapshot.sqlite"))?;
    let mut bytes = 0u64;
    for hash in &s.hashes {
        let dst = vault::object_path(&root, hash);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        decode(&repo, hash, &dst)?;
        bytes += dst.metadata()?.len();
    }
    ensure!(bytes == s.objects_bytes, "快照原件大小不一致");
    let manifest = json!({"format_version":bundle::FORMAT_VERSION,"app_version":env!("CARGO_PKG_VERSION"),
        "schema_version":s.schema_version,"created_at":s.created_at,"db_snapshot":"snapshot.sqlite",
        "objects":s.hashes.len(),"objects_bytes":bytes,"indexes_omitted":true,"filter":null});
    std::fs::write(root.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    let encoder =
        flate2::write::GzEncoder::new(pending.as_file_mut(), flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    tar.append_dir_all("snapshot", &root)?;
    tar.into_inner()?.finish()?;
    pending.as_file().sync_all()?;
    ensure!(
        bundle::verify(pending.path())?["ok"] == true,
        "导出备份校验失败"
    );
    pending.persist(out).map_err(|e| e.error)?;
    Ok(json!({"path":out}))
}
fn plan(repo: &Path, recent: usize, monthly: usize) -> Result<Value> {
    ensure!(
        (1..=1000).contains(&recent) && monthly <= 120,
        "至少保留最近 1 份；最近份数最多 1000，每月份数最多 120"
    );
    let rows = manifests(repo)?;
    let files = inventory(repo)?;
    let mut months = BTreeSet::new();
    let mut keep = BTreeSet::new();
    let mut remove = Vec::new();
    let tmp = tempfile::tempdir()?;
    let mut reclaim = 0u64;
    for (i, s) in rows.iter().enumerate() {
        inspect_db(repo, s, &tmp.path().join("db.sqlite"))?;
        ensure!(
            files.contains_key(&s.db_hash) && s.hashes.iter().all(|h| files.contains_key(h)),
            "快照引用对象缺失，已停止清理"
        );
        let month = &s.created_at[..7];
        let monthly_keep = months.len() < monthly && months.insert(month.to_string());
        if i < recent || monthly_keep {
            keep.insert(s.db_hash.clone());
            keep.extend(s.hashes.iter().cloned());
        } else {
            remove.push(s.id.clone());
            reclaim += repo
                .join("snapshots")
                .join(format!("{}.json", s.id))
                .metadata()?
                .len();
        }
    }
    let unused: Vec<_> = files
        .iter()
        .filter(|(hash, _)| !keep.contains(*hash))
        .map(|(h, n)| {
            reclaim += n;
            h.clone()
        })
        .collect();
    let token = vault::hash_bytes(&serde_json::to_vec(&(
        repo.canonicalize()?,
        &rows,
        &files,
        recent,
        monthly,
    ))?);
    Ok(
        json!({"token":token,"remove_count":remove.len(),"remove_ids":remove,"reclaim_bytes":reclaim,"unused":unused}),
    )
}
pub fn cleanup_plan(home: &Path, recent: usize, monthly: usize) -> Result<Value> {
    let repo = root(home);
    let _lock = lock(&repo)?;
    plan(&repo, recent, monthly)
}
pub fn cleanup(home: &Path, recent: usize, monthly: usize, token: &str) -> Result<Value> {
    let repo = root(home);
    let _lock = lock(&repo)?;
    let p = plan(&repo, recent, monthly)?;
    ensure!(
        p["token"].as_str() == Some(token),
        "备份已变化，请重新预览清理计划"
    );
    // Remove manifests first; a crash only leaves extra blobs for the next sweep.
    for id in p["remove_ids"].as_array().unwrap() {
        std::fs::remove_file(
            repo.join("snapshots")
                .join(format!("{}.json", id.as_str().unwrap())),
        )?;
    }
    for hash in p["unused"].as_array().unwrap() {
        std::fs::remove_file(blob(&repo, hash.as_str().unwrap()))?;
    }
    let mut pending = tempfile::NamedTempFile::new_in(&repo)?;
    serde_json::to_writer(
        pending.as_file_mut(),
        &json!({"keep_recent":recent,"keep_monthly":monthly}),
    )?;
    pending.as_file().sync_all()?;
    pending
        .persist(repo.join("policy.json"))
        .map_err(|e| e.error)?;
    Ok(json!({"removed":p["remove_count"],"reclaimed_bytes":p["reclaim_bytes"]}))
}
