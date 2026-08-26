//! Durable state on SpaceDB: drift, NTS cookies, and server master keys.
//!
//! Two house laws shape this module (mission plan §4):
//!
//! * **Per-entry, never a blob.** Each cookie, each drift reading, each master
//!   key is its own entry under its own compound key. chrony's driftfile and
//!   ntsdumpdir are single mutable files; a mesh replica cannot merge those,
//!   and a torn write loses everything. Per-entry CRDT registers merge, and a
//!   damaged entry costs one cookie.
//! * **Encrypted at rest.** Values are sealed with AES-256-GCM under an
//!   Argon2id-derived key, so the replica holds ciphertext even though the SDK
//!   is content-agnostic. Cookies and master keys are live credentials: a
//!   cookie discloses session keys, a master key forges every cookie we ever
//!   minted. The compound key is the AEAD's associated data, so a sealed value
//!   cannot be lifted from one entry to another.
//!
//! Boot order is deliberately *not* storage-first: the daemon must serve a
//! usable clock before the replica opens, so callers read the OS clock, start
//! discipline with defaults, then fold in what this store returns.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;
use rusty_time_nts::aead::NtsKeys;
use spacedb_sdk::{
    Capability, CrdtType, Database, Identity, Ops, Schema, Scope, Session, SignedCapability, Tier,
};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Collections. One per kind of thing we persist, so a retired cookie can
/// never take drift history down with it.
const C_DRIFT: &str = "rusty_time.drift";
const C_COOKIES: &str = "rusty_time.nts_cookies";
const C_KEYS: &str = "rusty_time.nts_keys";
const COLLECTIONS: [&str; 3] = [C_DRIFT, C_COOKIES, C_KEYS];

const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 16;
/// Master keys live in a small fixed slot ring, so the whole set can be
/// enumerated on boot without persisting an index (which would be a blob).
pub const MASTER_KEY_SLOTS: usize = 3;
/// Bound on cookies stored per server — also the read loop's terminator.
const MAX_COOKIES_PER_SERVER: usize = 64;

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Crypto(&'static str),
    Sdk(String),
    /// The file exists but is not one of ours, or is truncated.
    Corrupt(&'static str),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "store I/O: {e}"),
            StoreError::Crypto(what) => write!(f, "store crypto: {what}"),
            StoreError::Sdk(e) => write!(f, "store: {e}"),
            StoreError::Corrupt(what) => write!(f, "store file is unusable: {what}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// One persisted drift reading for a source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftRecord {
    pub freq_ppm: f64,
    pub offset_s: f64,
    pub updated_unix: u64,
}

/// A persisted NTS server master key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredMasterKey {
    pub id: u32,
    pub key: [u8; 32],
}

/// A resumable NTS client session, complete enough to skip key establishment.
pub struct StoredClientSession {
    pub keys: NtsKeys,
    pub cookies: Vec<Vec<u8>>,
    pub ntp_server: String,
    pub ntp_port: u16,
}

/// Budget granted to each per-operation session, in micro-$MATA. A write costs
/// ~2000, so this is generous for one op while still being a real bound: an
/// operation that somehow loops will hit a ceiling instead of running forever.
const SESSION_BUDGET_MICRO_MATA: u64 = 1_000_000;

pub struct Store {
    db: Database,
    /// Signed grants, one per collection. Sessions are minted per operation
    /// from these rather than held: a Session spends from its grant's budget,
    /// so a long-lived session in a daemon that runs for months would
    /// eventually refuse writes.
    capabilities: HashMap<&'static str, SignedCapability>,
    /// Field names declared per collection. SpaceDB schemas are explicit, so a
    /// compound key must be declared before it can be written or read; the set
    /// is rebuilt from the keys themselves, never persisted separately.
    fields: HashMap<&'static str, BTreeSet<String>>,
    path: PathBuf,
    aead: Aes256Gcm,
}

impl Store {
    /// Open (or create) the store at `path`, unlocking it with `passphrase`.
    pub fn open(path: impl AsRef<Path>, passphrase: &[u8]) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        // The salt lives beside the data so the same passphrase yields the same
        // key across restarts. A salt is not a secret; it exists to stop one
        // rainbow table covering every rusty_time store in the world.
        let salt_path = path.with_extension("salt");
        let salt: [u8; SALT_LEN] = if salt_path.exists() {
            let raw = std::fs::read(&salt_path)?;
            raw.try_into()
                .map_err(|_| StoreError::Corrupt("salt file is the wrong length"))?
        } else {
            let mut s = [0u8; SALT_LEN];
            rusty_time_nts::ke::fill_random(&mut s)
                .map_err(|_| StoreError::Crypto("no system randomness for salt"))?;
            std::fs::write(&salt_path, s)?;
            s
        };

        let mut key = [0u8; 32];
        Argon2::default()
            .hash_password_into(passphrase, &salt, &mut key)
            .map_err(|_| StoreError::Crypto("Argon2id key derivation failed"))?;
        let aead = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| StoreError::Crypto("AES-256-GCM rejected the derived key"))?;

        let owner = Identity::generate("did:mata:rusty-time")
            .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
        let node = Identity::generate("did:mata:rusty-time-node")
            .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
        let owner_did = owner.did().clone();
        let mut db = Database::open(node);
        db.register_identity(&owner)
            .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;

        let mut capabilities = HashMap::new();
        let mut fields = HashMap::new();
        for collection in COLLECTIONS {
            db.define(Schema::new(collection));
            fields.insert(collection, BTreeSet::new());
            let mut grant = Capability::grant(
                owner_did.clone(),
                owner_did.clone(),
                Scope::Collection(collection.to_string()),
                Ops::READ | Ops::WRITE,
            )
            .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
            grant.budget_micro_mata = Some(SESSION_BUDGET_MICRO_MATA);
            let signed = SignedCapability::sign(grant, &owner)
                .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
            capabilities.insert(collection, signed);
        }

        let mut store = Store {
            db,
            capabilities,
            fields,
            path,
            aead,
        };
        store.load_from_disk()?;
        Ok(store)
    }

    /// Declare a compound key as a field before touching it. SpaceDB schemas
    /// are explicit by design — an undeclared field is an error, not an
    /// implicit create — so keys are registered as they are used.
    fn ensure_field(&mut self, collection: &'static str, field: &str) {
        let set = self.fields.entry(collection).or_default();
        if set.contains(field) {
            return;
        }
        set.insert(field.to_string());
        let mut schema = Schema::new(collection);
        for name in set.iter() {
            schema = schema.field(name.clone(), CrdtType::Register, Tier::Convergent);
        }
        // `define` replaces the schema only; documents are untouched.
        self.db.define(schema);
    }

    /// A fresh session for one operation, carrying a fresh budget.
    fn session(&self, collection: &'static str) -> Result<Session, StoreError> {
        let capability = self
            .capabilities
            .get(collection)
            .ok_or(StoreError::Sdk("no capability for collection".into()))?;
        Ok(self.db.session(capability.clone()))
    }

    /// Write a value verbatim (already sealed, or the empty tombstone).
    fn put_raw(
        &mut self,
        collection: &'static str,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        self.ensure_field(collection, key);
        let mut session = self.session(collection)?;
        self.db
            .put_register(&mut session, collection, key, value)
            .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
        Ok(())
    }

    fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<String, StoreError> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rusty_time_nts::ke::fill_random(&mut nonce_bytes)
            .map_err(|_| StoreError::Crypto("no system randomness for nonce"))?;
        let nonce = Nonce::from(nonce_bytes);
        let ciphertext = self
            .aead
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| StoreError::Crypto("AES-256-GCM seal failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(hex(&out))
    }

    fn open_value(&self, stored: &str, aad: &[u8]) -> Result<Vec<u8>, StoreError> {
        let raw = unhex(stored).ok_or(StoreError::Corrupt("value is not hex"))?;
        if raw.len() < NONCE_LEN + 16 {
            return Err(StoreError::Corrupt("value is too short to be sealed"));
        }
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes.copy_from_slice(&raw[..NONCE_LEN]);
        self.aead
            .decrypt(
                &Nonce::from(nonce_bytes),
                Payload {
                    msg: &raw[NONCE_LEN..],
                    aad,
                },
            )
            .map_err(|_| StoreError::Crypto("value failed authentication"))
    }

    /// The compound key an entry lives under, and the AEAD's associated data.
    fn compound_key(kind: &str, owner: &str, index: usize) -> String {
        format!("{kind}/{owner}/{index}")
    }

    fn put_sealed(
        &mut self,
        collection: &'static str,
        key: &str,
        plaintext: &[u8],
    ) -> Result<(), StoreError> {
        let sealed = self.seal(plaintext, key.as_bytes())?;
        self.put_raw(collection, key, &sealed)
    }

    fn get_sealed(
        &mut self,
        collection: &'static str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.ensure_field(collection, key);
        let value = {
            let mut session = self.session(collection)?;
            let (value, _) = self
                .db
                .read_register(&mut session, collection, key)
                .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
            value
        };
        match value {
            None => Ok(None),
            // Empty string is our tombstone: the entry existed and was retired.
            Some(v) if v.is_empty() => Ok(Some(Vec::new())),
            Some(v) => self.open_value(&v, key.as_bytes()).map(Some),
        }
    }

    pub fn put_drift(&mut self, source: &str, record: DriftRecord) -> Result<(), StoreError> {
        let key = Self::compound_key("drift", source, 0);
        let payload = format!(
            "{} {} {}",
            record.freq_ppm, record.offset_s, record.updated_unix
        );
        self.put_sealed(C_DRIFT, &key, payload.as_bytes())
    }

    pub fn get_drift(&mut self, source: &str) -> Result<Option<DriftRecord>, StoreError> {
        let key = Self::compound_key("drift", source, 0);
        let Some(plain) = self.get_sealed(C_DRIFT, &key)? else {
            return Ok(None);
        };
        if plain.is_empty() {
            return Ok(None);
        }
        let text =
            core::str::from_utf8(&plain).map_err(|_| StoreError::Corrupt("drift is not utf-8"))?;
        let mut parts = text.split_whitespace();
        let mut next_num = || -> Result<f64, StoreError> {
            parts
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or(StoreError::Corrupt("drift field is not a number"))
        };
        Ok(Some(DriftRecord {
            freq_ppm: next_num()?,
            offset_s: next_num()?,
            updated_unix: next_num()? as u64,
        }))
    }

    /// Replace the cookie store for one NTS server. Each cookie is its own
    /// entry: a torn write loses one cookie, not the association.
    pub fn put_cookies(&mut self, server: &str, cookies: &[Vec<u8>]) -> Result<(), StoreError> {
        let keep = cookies.len().min(MAX_COOKIES_PER_SERVER);
        for (i, cookie) in cookies.iter().take(keep).enumerate() {
            let key = Self::compound_key("cookie", server, i);
            self.put_sealed(C_COOKIES, &key, cookie)?;
        }
        // Tombstone the tail: a shorter batch must not leave stale cookies a
        // later load would try to spend (the server rejects them, wasting an
        // exchange and a real cookie).
        for i in keep..MAX_COOKIES_PER_SERVER {
            let key = Self::compound_key("cookie", server, i);
            match self.get_sealed(C_COOKIES, &key)? {
                None => break,
                Some(v) if v.is_empty() => break, // already a tombstone
                Some(_) => self.put_raw(C_COOKIES, &key, "")?,
            }
        }
        Ok(())
    }

    pub fn get_cookies(&mut self, server: &str) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut out = Vec::new();
        for i in 0..MAX_COOKIES_PER_SERVER {
            let key = Self::compound_key("cookie", server, i);
            match self.get_sealed(C_COOKIES, &key)? {
                None => break,
                Some(v) if v.is_empty() => break, // tombstone
                Some(v) => out.push(v),
            }
        }
        Ok(out)
    }

    /// Save a resumable NTS client session: keys, unspent cookies, and the
    /// endpoint KE pointed us at.
    ///
    /// These three travel together on purpose. A cookie tells the *server* what
    /// the session keys were; the client needs its own C2S/S2C copy to protect
    /// and verify. Saving cookies without keys produces a state file that can
    /// never be resumed from — and the failure would only surface much later,
    /// as an unexplained NTS-KE round on every start.
    pub fn put_client_session(
        &mut self,
        ke_host: &str,
        keys: &NtsKeys,
        cookies: &[Vec<u8>],
        ntp_server: &str,
        ntp_port: u16,
    ) -> Result<(), StoreError> {
        let key_field = Self::compound_key("client_keys", ke_host, 0);
        let mut payload = Vec::with_capacity(64);
        payload.extend_from_slice(&keys.c2s);
        payload.extend_from_slice(&keys.s2c);
        self.put_sealed(C_KEYS, &key_field, &payload)?;

        let meta_field = Self::compound_key("client_meta", ke_host, 0);
        let meta = format!("{ntp_server} {ntp_port}");
        self.put_sealed(C_KEYS, &meta_field, meta.as_bytes())?;

        self.put_cookies(ke_host, cookies)
    }

    /// Restore a resumable NTS client session, if one is stored and complete.
    /// Returns `None` when any part is missing — a partial session is not
    /// usable, and pretending otherwise would spend cookies against keys we do
    /// not have.
    pub fn get_client_session(
        &mut self,
        ke_host: &str,
    ) -> Result<Option<StoredClientSession>, StoreError> {
        let key_field = Self::compound_key("client_keys", ke_host, 0);
        let Some(raw) = self.get_sealed(C_KEYS, &key_field)? else {
            return Ok(None);
        };
        if raw.len() != 64 {
            return Ok(None);
        }
        let mut keys = NtsKeys {
            c2s: [0u8; 32],
            s2c: [0u8; 32],
        };
        keys.c2s.copy_from_slice(&raw[..32]);
        keys.s2c.copy_from_slice(&raw[32..]);

        let meta_field = Self::compound_key("client_meta", ke_host, 0);
        let Some(meta_raw) = self.get_sealed(C_KEYS, &meta_field)? else {
            return Ok(None);
        };
        let meta = core::str::from_utf8(&meta_raw)
            .map_err(|_| StoreError::Corrupt("client meta is not utf-8"))?;
        let mut parts = meta.split_whitespace();
        let (Some(server), Some(port)) = (parts.next(), parts.next()) else {
            return Ok(None);
        };
        let Ok(port) = port.parse::<u16>() else {
            return Ok(None);
        };

        let cookies = self.get_cookies(ke_host)?;
        if cookies.is_empty() {
            return Ok(None);
        }
        Ok(Some(StoredClientSession {
            keys,
            cookies,
            ntp_server: server.to_string(),
            ntp_port: port,
        }))
    }

    /// Persist a server master key in `slot`, so cookies minted before a
    /// restart still redeem afterwards. Without this, every restart strands
    /// every client holding a cookie.
    pub fn put_master_key(&mut self, slot: usize, key: &StoredMasterKey) -> Result<(), StoreError> {
        let field = Self::compound_key("master", "self", slot % MASTER_KEY_SLOTS);
        let mut payload = Vec::with_capacity(36);
        payload.extend_from_slice(&key.id.to_be_bytes());
        payload.extend_from_slice(&key.key);
        self.put_sealed(C_KEYS, &field, &payload)
    }

    pub fn get_master_key(&mut self, slot: usize) -> Result<Option<StoredMasterKey>, StoreError> {
        let field = Self::compound_key("master", "self", slot % MASTER_KEY_SLOTS);
        let Some(plain) = self.get_sealed(C_KEYS, &field)? else {
            return Ok(None);
        };
        if plain.len() != 36 {
            return Ok(None);
        }
        let id = u32::from_be_bytes([plain[0], plain[1], plain[2], plain[3]]);
        let mut key = [0u8; 32];
        key.copy_from_slice(&plain[4..]);
        Ok(Some(StoredMasterKey { id, key }))
    }

    /// Every master key still held, oldest slot first.
    pub fn all_master_keys(&mut self) -> Result<Vec<StoredMasterKey>, StoreError> {
        let mut out = Vec::new();
        for slot in 0..MASTER_KEY_SLOTS {
            if let Some(k) = self.get_master_key(slot)? {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Flush to disk as a per-entry record log.
    ///
    /// **Why not the CRDT's own `export`/`import`.** Those exist for *mesh
    /// sync*, where two live replicas exchange updates. Using them as the local
    /// durability format meant every restart re-imported an update produced by
    /// a previous import, and that accumulation reliably tripped a
    /// divide-by-zero in yrs 0.25.0's `find_pivot` on the third cycle — see
    /// `import_guarded` and the `client_session_survives_repeated_cycles` test.
    ///
    /// A record log is also a closer fit to the house law: each entry is
    /// already its own sealed value under its own compound key, so writing them
    /// out individually is the natural encoding, and one damaged record costs
    /// one entry rather than the whole file. `export`/`import` remain in use for
    /// their real purpose when this store joins a mesh.
    ///
    /// Layout, repeated: `u16 collection_index | u32 key_len | key | u32 value_len | value`.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        let mut blob = Vec::new();
        // Snapshot the key sets first: reading values borrows `self`.
        let plan: Vec<(usize, Vec<String>)> = COLLECTIONS
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    i,
                    self.fields
                        .get(c)
                        .map(|s| s.iter().cloned().collect())
                        .unwrap_or_default(),
                )
            })
            .collect();

        for (index, keys) in plan {
            let collection = COLLECTIONS[index];
            for key in keys {
                let Some(value) = self.get_raw(collection, &key)? else {
                    continue;
                };
                blob.extend_from_slice(&(index as u16).to_be_bytes());
                blob.extend_from_slice(&(key.len() as u32).to_be_bytes());
                blob.extend_from_slice(key.as_bytes());
                blob.extend_from_slice(&(value.len() as u32).to_be_bytes());
                blob.extend_from_slice(value.as_bytes());
            }
        }

        // Write-then-rename: a crash mid-write must not truncate the store.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &blob)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// The stored (still sealed) value for a key, if present.
    fn get_raw(
        &mut self,
        collection: &'static str,
        key: &str,
    ) -> Result<Option<String>, StoreError> {
        self.ensure_field(collection, key);
        let mut session = self.session(collection)?;
        let (value, _) = self
            .db
            .read_register(&mut session, collection, key)
            .map_err(|e| StoreError::Sdk(format!("{e:?}")))?;
        Ok(value)
    }

    /// Merge another replica's state into this one.
    ///
    /// This is what SpaceDB's `export`/`import` are actually for — two live
    /// replicas exchanging CRDT updates — as distinct from local durability,
    /// which is the record log in [`Store::flush`]. When this store joins the
    /// mesh, this is the seam that carries a peer's drift history in.
    pub fn merge_from(&mut self, other: &Store) -> Result<(), StoreError> {
        for collection in COLLECTIONS {
            let update = other.db.export(collection);
            if !update.is_empty() {
                self.import_guarded(collection, &update)?;
            }
        }
        Ok(())
    }

    /// Import one collection, surviving a panic from the CRDT layer.
    ///
    /// **Why this guard exists.** `yrs` 0.25.0 (reached through spacedb-sdk)
    /// seeds an interpolation search with `clock / end` in
    /// `block_store.rs:51`'s `find_pivot` and does not guard `end == 0`, so
    /// certain accumulated states panic with "attempt to divide by zero" on
    /// import. Reproduced deterministically by
    /// `client_session_survives_repeated_cycles`.
    ///
    /// A time daemon must not die because its own cache is unlucky. Everything
    /// in this store is recoverable — cookies and drift are re-measured, and a
    /// lost master key only forces clients to re-run NTS-KE, which is what
    /// happens without persistence anyway. So a panicking import degrades that
    /// collection to empty, loudly, instead of taking the process down.
    fn import_guarded(
        &mut self,
        collection: &'static str,
        update: &[u8],
    ) -> Result<(), StoreError> {
        let db = &mut self.db;
        // The default hook would print a confusing backtrace for something we
        // are deliberately handling; restore it immediately afterwards.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.import(collection, update)
        }));
        std::panic::set_hook(previous_hook);

        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(StoreError::Sdk(format!("{e:?}"))),
            Err(_) => {
                eprintln!(
                    "rusty_time: state for '{collection}' could not be replayed (CRDT layer \
                     panicked; known yrs 0.25.0 find_pivot divide-by-zero). Continuing with that \
                     collection empty — cookies and drift will be re-measured."
                );
                Ok(())
            }
        }
    }

    fn load_from_disk(&mut self) -> Result<(), StoreError> {
        if !self.path.exists() {
            return Ok(());
        }
        let blob = std::fs::read(&self.path)?;
        let mut at = 0usize;

        let take = |at: &mut usize, n: usize| -> Result<&[u8], StoreError> {
            let end = at
                .checked_add(n)
                .filter(|e| *e <= blob.len())
                .ok_or(StoreError::Corrupt("store file ended mid-record"))?;
            let slice = &blob[*at..end];
            *at = end;
            Ok(slice)
        };

        while at < blob.len() {
            let index = {
                let b = take(&mut at, 2)?;
                u16::from_be_bytes([b[0], b[1]]) as usize
            };
            let collection = *COLLECTIONS
                .get(index)
                .ok_or(StoreError::Corrupt("store names an unknown collection"))?;
            let key_len = {
                let b = take(&mut at, 4)?;
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
            };
            let key = core::str::from_utf8(take(&mut at, key_len)?)
                .map_err(|_| StoreError::Corrupt("entry key is not utf-8"))?
                .to_string();
            let value_len = {
                let b = take(&mut at, 4)?;
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
            };
            let value = core::str::from_utf8(take(&mut at, value_len)?)
                .map_err(|_| StoreError::Corrupt("entry value is not utf-8"))?
                .to_string();

            // Replay as a normal write: the CRDT is rebuilt from entries, so no
            // update produced by a previous import is ever re-imported.
            self.put_raw(collection, &key, &value)?;
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rusty_time_store_{name}.spacedb"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("salt"));
        p
    }

    #[test]
    fn drift_survives_a_restart() {
        let path = temp_path("drift");
        let record = DriftRecord {
            freq_ppm: -12.5,
            offset_s: 0.000_25,
            updated_unix: 1_756_224_000,
        };
        {
            let mut s = Store::open(&path, b"correct horse battery staple").expect("open");
            s.put_drift("pool.ntp.org", record).expect("put");
            s.flush().expect("flush");
        }
        let mut s = Store::open(&path, b"correct horse battery staple").expect("reopen");
        let back = s.get_drift("pool.ntp.org").expect("get").expect("present");
        assert!((back.freq_ppm - record.freq_ppm).abs() < 1e-9);
        assert!((back.offset_s - record.offset_s).abs() < 1e-12);
        assert_eq!(back.updated_unix, record.updated_unix);
        assert_eq!(s.get_drift("other.server").expect("get"), None);
    }

    #[test]
    fn cookies_round_trip_and_shrink_cleanly() {
        let path = temp_path("cookies");
        let mut s = Store::open(&path, b"pass").expect("open");
        let batch: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 40]).collect();
        s.put_cookies("time.example", &batch).expect("put");
        assert_eq!(s.get_cookies("time.example").expect("get"), batch);

        // A shorter batch must not leave the old tail readable.
        let short: Vec<Vec<u8>> = (0..2u8).map(|i| vec![i + 100; 40]).collect();
        s.put_cookies("time.example", &short).expect("put short");
        assert_eq!(s.get_cookies("time.example").expect("get"), short);
    }

    #[test]
    fn master_keys_survive_a_restart() {
        let path = temp_path("master");
        let k0 = StoredMasterKey {
            id: 11,
            key: [0x5A; 32],
        };
        let k1 = StoredMasterKey {
            id: 12,
            key: [0x6B; 32],
        };
        {
            let mut s = Store::open(&path, b"pass").expect("open");
            s.put_master_key(0, &k0).expect("put 0");
            s.put_master_key(1, &k1).expect("put 1");
            s.flush().expect("flush");
        }
        let mut s = Store::open(&path, b"pass").expect("reopen");
        assert_eq!(s.all_master_keys().expect("all"), vec![k0, k1]);
    }

    #[test]
    fn wrong_passphrase_cannot_read_values() {
        let path = temp_path("wrongpass");
        {
            let mut s = Store::open(&path, b"right").expect("open");
            s.put_master_key(
                0,
                &StoredMasterKey {
                    id: 1,
                    key: [9u8; 32],
                },
            )
            .expect("put");
            s.flush().expect("flush");
        }
        let mut s = Store::open(&path, b"wrong").expect("reopen");
        match s.get_master_key(0) {
            Err(StoreError::Crypto(_)) => {}
            other => panic!("wrong passphrase yielded {other:?}"),
        }
    }

    #[test]
    fn secrets_never_touch_the_disk_in_clear() {
        let path = temp_path("plaintext");
        let secret = [0xABu8; 32];
        let cookie = vec![0xCDu8; 48];
        {
            let mut s = Store::open(&path, b"pass").expect("open");
            s.put_master_key(0, &StoredMasterKey { id: 3, key: secret })
                .expect("put key");
            s.put_cookies("srv", std::slice::from_ref(&cookie))
                .expect("put cookie");
            s.flush().expect("flush");
        }
        let raw = std::fs::read(&path).expect("read file");
        assert!(
            !raw.windows(secret.len()).any(|w| w == secret),
            "master key found in cleartext on disk"
        );
        assert!(
            !raw.windows(cookie.len()).any(|w| w == cookie.as_slice()),
            "NTS cookie found in cleartext on disk"
        );
    }

    #[test]
    fn a_sealed_value_cannot_be_moved_to_another_key() {
        // The compound key is the AEAD's associated data, so lifting one
        // server's sealed cookie onto another server's entry must not decrypt.
        let path = temp_path("aad");
        let s = Store::open(&path, b"pass").expect("open");
        let sealed = s.seal(b"secret-value", b"cookie/serverA/0").expect("seal");
        assert!(s.open_value(&sealed, b"cookie/serverA/0").is_ok());
        match s.open_value(&sealed, b"cookie/serverB/0") {
            Err(StoreError::Crypto(_)) => {}
            other => panic!("value moved across keys: {:?}", other.map(|_| "decrypted")),
        }
    }

    #[test]
    fn survives_repeated_open_flush_cycles() {
        // The daemon opens, writes and flushes on every run, so the store must
        // survive an unbounded number of round trips. The third cycle is where
        // an export-of-an-imported-document first went wrong.
        let path = temp_path("cycles");
        for cycle in 0..6u32 {
            let mut s = Store::open(&path, b"pass")
                .unwrap_or_else(|e| panic!("open failed on cycle {cycle}: {e}"));
            s.put_drift(
                "server",
                DriftRecord {
                    freq_ppm: cycle as f64,
                    offset_s: 0.001,
                    updated_unix: 1_756_224_000 + cycle as u64,
                },
            )
            .unwrap_or_else(|e| panic!("put failed on cycle {cycle}: {e}"));
            s.flush()
                .unwrap_or_else(|e| panic!("flush failed on cycle {cycle}: {e}"));
            let back = s
                .get_drift("server")
                .unwrap_or_else(|e| panic!("get failed on cycle {cycle}: {e}"))
                .unwrap_or_else(|| panic!("drift missing on cycle {cycle}"));
            assert!((back.freq_ppm - cycle as f64).abs() < 1e-9, "cycle {cycle}");
        }
    }

    #[test]
    fn client_session_survives_repeated_cycles() {
        // The exact sequence `rtimed query --state` performs each run: save a
        // full client session, flush, reopen, resume, save again.
        let path = temp_path("client_cycles");
        let keys = NtsKeys {
            c2s: [1u8; 32],
            s2c: [2u8; 32],
        };
        let cookies: Vec<Vec<u8>> = (0..9u8).map(|i| vec![i; 100]).collect();
        for cycle in 0..4u32 {
            let mut s = Store::open(&path, b"pass")
                .unwrap_or_else(|e| panic!("open failed on cycle {cycle}: {e}"));
            if cycle > 0 {
                let resumed = s
                    .get_client_session("ke.example")
                    .unwrap_or_else(|e| panic!("resume failed on cycle {cycle}: {e}"))
                    .unwrap_or_else(|| panic!("session missing on cycle {cycle}"));
                assert_eq!(resumed.cookies.len(), cookies.len(), "cycle {cycle}");
                assert_eq!(resumed.ntp_port, 123);
            }
            s.put_client_session("ke.example", &keys, &cookies, "ntp.example", 123)
                .unwrap_or_else(|e| panic!("save failed on cycle {cycle}: {e}"));
            s.flush()
                .unwrap_or_else(|e| panic!("flush failed on cycle {cycle}: {e}"));
        }
    }

    #[test]
    fn merging_a_peer_replica_carries_its_entries() {
        // The mesh path: another replica's CRDT state merges in, and its
        // entries become readable here. Both stores share a passphrase and
        // salt, as replicas of one owner's data would.
        let path_a = temp_path("merge_a");
        let path_b = temp_path("merge_b");
        // Share the salt so both derive the same at-rest key.
        let mut a = Store::open(&path_a, b"pass").expect("open a");
        std::fs::copy(path_a.with_extension("salt"), path_b.with_extension("salt"))
            .expect("share salt");
        let mut b = Store::open(&path_b, b"pass").expect("open b");

        a.put_drift(
            "peer.example",
            DriftRecord {
                freq_ppm: 3.5,
                offset_s: 0.002,
                updated_unix: 1_756_224_000,
            },
        )
        .expect("put on a");

        b.merge_from(&a).expect("merge");
        // b must now declare the field it just received before it can read it.
        let got = b.get_drift("peer.example").expect("get on b");
        assert_eq!(
            got.map(|d| d.freq_ppm),
            Some(3.5),
            "peer's entry did not survive the merge"
        );
    }

    #[test]
    fn truncated_file_is_reported_not_silently_ignored() {
        let path = temp_path("corrupt");
        {
            let mut s = Store::open(&path, b"pass").expect("open");
            s.put_master_key(
                0,
                &StoredMasterKey {
                    id: 1,
                    key: [1u8; 32],
                },
            )
            .expect("put");
            s.flush().expect("flush");
        }
        let raw = std::fs::read(&path).expect("read");
        std::fs::write(&path, &raw[..raw.len() / 2]).expect("truncate");
        match Store::open(&path, b"pass") {
            Err(StoreError::Corrupt(_)) | Err(StoreError::Sdk(_)) => {}
            Ok(_) => panic!("truncated store opened as if healthy"),
            Err(e) => panic!("unexpected error {e}"),
        }
    }
}
