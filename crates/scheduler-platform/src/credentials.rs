use std::{fs, path::PathBuf};

#[cfg(not(windows))]
use anyhow::bail;
use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use scheduler_core::{ClientProof, CredentialRef};
use sha2::Sha256;
use zeroize::Zeroize;

use crate::DataPaths;

#[derive(Clone)]
pub struct SecretKey([u8; 32]);
impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl SecretKey {
    pub fn random() -> Self {
        let mut bytes = [0; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(bytes)
    }
    pub fn sign(&self, label: &str, data: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts 32-byte keys");
        mac.update(label.as_bytes());
        mac.update(&[0]);
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }
    pub fn verify(&self, label: &str, data: &[u8], proof: &str) -> bool {
        let Ok(proof) = hex::decode(proof) else {
            return false;
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts 32-byte keys");
        mac.update(label.as_bytes());
        mac.update(&[0]);
        mac.update(data);
        mac.verify_slice(&proof).is_ok()
    }
    pub fn ack_nonce(&self, delivery_id: &str) -> String {
        self.sign("delivery-ack-v1", delivery_id.as_bytes())
    }
}

pub fn proof_payload(server_nonce: &str, proof: &ClientProof) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        proof.version,
        &proof.credential,
        &proof.origin,
        server_nonce,
        &proof.client_nonce,
    ))?)
}

#[derive(Clone)]
pub struct CredentialStore {
    paths: DataPaths,
}
impl CredentialStore {
    pub fn new(paths: DataPaths) -> Self {
        Self { paths }
    }
    fn path(&self, reference: &CredentialRef) -> Result<PathBuf> {
        let file = match reference {
            CredentialRef::Admin => "admin.dpapi".to_owned(),
            CredentialRef::Project { project_id } => {
                uuid::Uuid::parse_str(project_id).context("Invalid project credential ID")?;
                format!("project-{project_id}.dpapi")
            }
        };
        Ok(self.paths.secrets.join(file))
    }
    pub fn load(&self, reference: &CredentialRef) -> Result<SecretKey> {
        let protected = fs::read(self.path(reference)?)
            .context("Project credentials are unavailable; initialize the project first")?;
        #[cfg(windows)]
        let mut bytes = crate::windows::unprotect(&protected)?;
        #[cfg(not(windows))]
        let mut bytes: Vec<u8> = {
            let _ = protected;
            bail!("Persistent credential backend is not enabled on this release target")
        };
        let key: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("Invalid protected credential length")?;
        bytes.zeroize();
        Ok(SecretKey(key))
    }
    pub fn ensure(&self, reference: &CredentialRef) -> Result<SecretKey> {
        let path = self.path(reference)?;
        if path.exists() {
            return self.load(reference);
        }
        let key = SecretKey::random();
        #[cfg(windows)]
        let protected = crate::windows::protect(&key.0)?;
        #[cfg(not(windows))]
        let protected: Vec<u8> =
            { bail!("Persistent credential backend is not enabled on this release target") };
        let staging = self
            .paths
            .secrets
            .join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let write = (|| -> Result<bool> {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)?;
            file.write_all(&protected)?;
            file.sync_all()?;
            match fs::hard_link(&staging, &path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error.into()),
            }
        })();
        let _ = fs::remove_file(&staging);
        if write? {
            Ok(key)
        } else {
            self.load(reference)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proof_binds_origin_and_rejects_reflection() {
        let key = SecretKey::random();
        let mut proof = ClientProof {
            version: 1,
            credential: CredentialRef::Project {
                project_id: uuid::Uuid::new_v4().to_string(),
            },
            origin: Some(scheduler_core::OriginSession {
                provider_session_id: "a".into(),
                source: scheduler_core::OriginSource::RequestMetadata,
            }),
            client_nonce: "client-nonce".into(),
            proof: String::new(),
        };
        let data = proof_payload("server-nonce", &proof).unwrap();
        let signature = key.sign("client", &data);
        assert!(key.verify("client", &data, &signature));
        assert!(!key.verify("server", &data, &signature));
        proof.origin.as_mut().unwrap().provider_session_id = "other".into();
        assert!(!key.verify(
            "client",
            &proof_payload("server-nonce", &proof).unwrap(),
            &signature
        ));
    }
    #[cfg(windows)]
    #[test]
    fn credentials_are_protected_and_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::prepare(dir.path()).unwrap();
        let store = CredentialStore::new(paths.clone());
        let key = store.ensure(&CredentialRef::Admin).unwrap();
        let signature = key.sign("test", b"content");
        assert!(
            store
                .load(&CredentialRef::Admin)
                .unwrap()
                .verify("test", b"content", &signature)
        );
        let raw = fs::read(paths.secrets.join("admin.dpapi")).unwrap();
        assert_ne!(raw, key.0);
        assert!(
            store
                .load(&CredentialRef::Project {
                    project_id: "../escape".into()
                })
                .is_err()
        );
    }
    #[cfg(windows)]
    #[test]
    fn concurrent_creation_never_rotates_an_existing_project_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::prepare(dir.path()).unwrap();
        let store = CredentialStore::new(paths);
        let reference = CredentialRef::Project {
            project_id: uuid::Uuid::new_v4().to_string(),
        };
        let copies: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let reference = reference.clone();
                std::thread::spawn(move || store.ensure(&reference).unwrap().sign("test", b"same"))
            })
            .collect();
        let expected = store.ensure(&reference).unwrap().sign("test", b"same");
        for copy in copies {
            assert_eq!(copy.join().unwrap(), expected);
        }
    }
}
