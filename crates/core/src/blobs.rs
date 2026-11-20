//! Content-addressed immutable blobs under `<root>/blobs` (file name = lowercase SHA-256 hex) and
//! the AssetStore views engines receive. Engines only ever see a [`StagingAssets`]: puts stay in
//! memory until the core commits them; reads fall through to durable canonical blobs.
use crate::errors::{integrity, invalid, io_store, not_found, storage};
use crate::fsx;
use designllm_contracts::{sha256, AssetRef, AssetStore, DltResult};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const TEMP_SUFFIX: &str = ".partial";

pub fn is_hash(name: &str) -> bool { name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) }

/// Durability points observable by fault-injection hooks (tests only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint { BlobTempWritten, BlobFinalized, TxnBeforeCommit, AfterCommit, NativeTempWritten, NativeAfterRename }
impl FaultPoint {
    pub const ALL: [FaultPoint; 6] = [Self::BlobTempWritten, Self::BlobFinalized, Self::TxnBeforeCommit, Self::AfterCommit, Self::NativeTempWritten, Self::NativeAfterRename];
    pub fn name(self) -> &'static str {
        match self {
            Self::BlobTempWritten => "blob_temp_written", Self::BlobFinalized => "blob_finalized",
            Self::TxnBeforeCommit => "txn_before_commit", Self::AfterCommit => "after_commit",
            Self::NativeTempWritten => "native_temp_written", Self::NativeAfterRename => "native_after_rename",
        }
    }
    pub fn parse(s: &str) -> Option<Self> { Self::ALL.into_iter().find(|p| p.name() == s) }
}
pub type FaultHook = Box<dyn FnMut(FaultPoint) -> DltResult<()> + Send>;
pub type SharedHook = Arc<Mutex<Option<FaultHook>>>;
pub fn fire(hook: &SharedHook, point: FaultPoint) -> DltResult<()> {
    let mut guard = hook.lock().map_err(|_| storage("fault hook poisoned"))?;
    match guard.as_mut() { Some(h) => h(point), None => Ok(()) }
}

#[derive(Debug, Default)]
pub struct BlobScan { pub blobs: Vec<String>, pub temps: Vec<PathBuf>, pub foreign: Vec<String> }

pub struct BlobStore { dir: PathBuf, counter: AtomicU64, hook: SharedHook }
impl BlobStore {
    pub fn open(dir: PathBuf, hook: SharedHook) -> DltResult<Self> {
        fs::create_dir_all(&dir).map_err(io_store)?;
        Ok(Self { dir, counter: AtomicU64::new(0), hook })
    }
    pub fn dir(&self) -> &std::path::Path { &self.dir }
    pub fn path_of(&self, hash: &str) -> PathBuf { self.dir.join(hash) }
    pub fn contains(&self, hash: &str) -> bool { is_hash(hash) && self.path_of(hash).is_file() }

    /// temp write, fsync, verify, no-replace write-through rename, fsync. The hash is returned
    /// only after the blob is durable; callers reference it in the DB afterwards.
    pub fn put(&self, bytes: &[u8]) -> DltResult<String> {
        let hash = sha256(bytes);
        let final_path = self.path_of(&hash);
        if final_path.is_file() {
            self.get(&hash)?;
            return Ok(hash);
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let temp = self.dir.join(format!("{}.{}.{n}{TEMP_SUFFIX}", &hash[..16], std::process::id()));
        let written = (|| -> io::Result<()> {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&temp)?;
            f.write_all(bytes)?;
            f.sync_all()
        })();
        if let Err(e) = written { let _ = fs::remove_file(&temp); return Err(io_store(e)); }
        match fs::read(&temp).map(|b| sha256(&b)) {
            Ok(h) if h == hash => {}
            Ok(_) => { let _ = fs::remove_file(&temp); return Err(integrity("blob temp file does not match its hash")); }
            Err(e) => { let _ = fs::remove_file(&temp); return Err(io_store(e)); }
        }
        if let Err(e) = fire(&self.hook, FaultPoint::BlobTempWritten) { let _ = fs::remove_file(&temp); return Err(e); }
        match fsx::rename_durable(&temp, &final_path, false) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => { let _ = fs::remove_file(&temp); self.get(&hash)?; return Ok(hash); }
            Err(e) => { let _ = fs::remove_file(&temp); return Err(io_store(e)); }
        }
        fsx::sync_file(&final_path).map_err(io_store)?;
        fire(&self.hook, FaultPoint::BlobFinalized)?;
        Ok(hash)
    }
    /// Reads and verifies the content hash.
    pub fn get(&self, hash: &str) -> DltResult<Vec<u8>> {
        if !is_hash(hash) { return Err(invalid(format!("invalid blob id {hash:?}"))); }
        let bytes = match fs::read(self.path_of(hash)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(integrity(format!("blob {hash} is missing"))),
            Err(e) => return Err(io_store(e)),
        };
        if sha256(&bytes) != hash { return Err(integrity(format!("blob {hash} content does not match its hash"))); }
        Ok(bytes)
    }
    pub fn read_range(&self, hash: &str, offset: u64, max: usize) -> DltResult<(Vec<u8>, u64)> {
        if !is_hash(hash) { return Err(invalid(format!("invalid blob id {hash:?}"))); }
        fsx::read_range(&self.path_of(hash), offset, max).map_err(|e| if e.kind() == io::ErrorKind::NotFound { integrity(format!("blob {hash} is missing")) } else { io_store(e) })
    }
    pub fn scan(&self) -> DltResult<BlobScan> {
        let mut scan = BlobScan::default();
        for entry in fs::read_dir(&self.dir).map_err(io_store)? {
            let entry = entry.map_err(io_store)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_file = entry.file_type().map_err(io_store)?.is_file();
            if is_file && is_hash(&name) { scan.blobs.push(name); }
            else if is_file && name.ends_with(TEMP_SUFFIX) { scan.temps.push(entry.path()); }
            else { scan.foreign.push(name); }
        }
        scan.blobs.sort();
        scan.foreign.sort();
        Ok(scan)
    }
}

/// Canonical assets: durable blobs plus the committed metadata index (mirrors the `assets` table).
pub struct AssetBase { pub blobs: BlobStore, index: RwLock<HashMap<String, AssetRef>> }
impl AssetBase {
    pub fn new(blobs: BlobStore, rows: Vec<AssetRef>) -> Self {
        Self { blobs, index: RwLock::new(rows.into_iter().map(|a| (a.sha256.clone(), a)).collect()) }
    }
    pub fn get_ref(&self, hash: &str) -> Option<AssetRef> { self.index.read().ok().and_then(|m| m.get(hash).cloned()) }
    pub fn add_committed(&self, refs: &[AssetRef]) {
        if let Ok(mut m) = self.index.write() { for r in refs { m.entry(r.sha256.clone()).or_insert_with(|| r.clone()); } }
    }
    pub fn read_canonical(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
        let known = self.get_ref(&asset.sha256).ok_or_else(|| not_found(format!("asset {} not found", asset.sha256)))?;
        let bytes = self.blobs.get(&known.sha256)?;
        if bytes.len() as u64 != known.bytes { return Err(integrity(format!("asset {} length differs from metadata", known.sha256))); }
        Ok(bytes)
    }
}

/// Engine-facing store. `put` never touches canonical storage; [`StagingAssets::take_staged`]
/// hands new assets to the commit path, otherwise they are dropped with this value.
pub struct StagingAssets { base: Arc<AssetBase>, staged: Mutex<BTreeMap<String, (AssetRef, Arc<Vec<u8>>)>> }
impl StagingAssets {
    pub fn new(base: Arc<AssetBase>) -> Self { Self { base, staged: Mutex::new(BTreeMap::new()) } }

    pub fn take_staged(&self) -> Vec<(AssetRef, Arc<Vec<u8>>)> { self.staged.lock().map(|mut m| std::mem::take(&mut *m).into_values().collect()).unwrap_or_default() }
    pub fn contains(&self, hash: &str) -> bool {
        self.base.get_ref(hash).is_some() || self.staged.lock().map(|m| m.contains_key(hash)).unwrap_or(false)
    }
}
impl AssetStore for StagingAssets {
    fn put(&self, bytes: &[u8], mime: &str) -> DltResult<AssetRef> {
        if mime.trim().is_empty() { return Err(invalid("asset mime is empty")); }
        let hash = sha256(bytes);
        if let Some(existing) = self.base.get_ref(&hash) { return Ok(existing); }
        let mut staged = self.staged.lock().map_err(|_| storage("staging store poisoned"))?;
        let entry = staged.entry(hash.clone()).or_insert_with(|| (AssetRef { sha256: hash, mime: mime.to_string(), bytes: bytes.len() as u64 }, Arc::new(bytes.to_vec())));
        Ok(entry.0.clone())
    }
    fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
        if !is_hash(&asset.sha256) { return Err(invalid(format!("invalid asset hash {:?}", asset.sha256))); }
        if let Some((_, bytes)) = self.staged.lock().map_err(|_| storage("staging store poisoned"))?.get(&asset.sha256) { return Ok(bytes.as_ref().clone()); }
        self.base.read_canonical(asset)
    }
    fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
        if !is_hash(hash) { return Err(invalid(format!("invalid asset hash {hash:?}"))); }
        if let Some((r, _)) = self.staged.lock().map_err(|_| storage("staging store poisoned"))?.get(hash) { return Ok(r.clone()); }
        self.base.get_ref(hash).ok_or_else(|| not_found(format!("asset {hash} not found")))
    }
}
