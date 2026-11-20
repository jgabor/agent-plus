//! The run's certificate authority, which the guest trusts.
//!
//! The network process makes one when it starts, before it confines itself,
//! and keeps its key until it ends. The key is never written anywhere. Only
//! the authority's certificate leaves the process, for the guest's init to
//! add to the guest's bundle.
//!
//! The authority signs a certificate for each name the proxy decrypts. Every
//! certificate it signs shares one key, made with the authority's, so issuing
//! one costs a signature and no key. `rcgen` writes the certificates, and
//! graviola makes the keys and signs.

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graviola::hashing::{Sha256, sha2::Sha256Context};
use graviola::signing::ecdsa::{P256, SigningKey as EcdsaKey};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyIdMethod, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PublicKeyData, SerialNumber,
    SignatureAlgorithm, SigningKey,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::OffsetDateTime;

/// How long before it is made a certificate is valid, so a guest whose
/// clock runs a little behind still accepts it.
const BEFORE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long after it is made a certificate is valid. A run's authority ends
/// with the run, so this only has to outlast the longest run.
const AFTER: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// The authority's name, as a program that shows certificates shows it.
const NAME: &str = "Agent+ run";

/// A run's certificate authority, with its key.
pub struct Authority {
    issuer: Issuer<'static, Key>,
    certificate: CertificateDer<'static>,
    pem: String,
    /// The key of every certificate the authority signs.
    leaf: Key,
}

/// A certificate the authority signed for a name, and its key.
pub struct Leaf {
    pub certificate: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl Authority {
    /// A new authority, valid around `now`.
    pub fn generate(now: SystemTime) -> io::Result<Authority> {
        let key = Key::generate()?;
        let leaf = Key::generate()?;
        let mut params = params(now)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, NAME);
        params.key_identifier_method = KeyIdMethod::PreSpecified(key.identifier());
        let certificate = params.self_signed(&key).map_err(invalid)?;
        Ok(Authority {
            issuer: Issuer::new(params, key),
            pem: certificate.pem(),
            certificate: certificate.der().clone(),
            leaf,
        })
    }

    /// The authority's certificate, in DER.
    pub fn certificate(&self) -> &CertificateDer<'static> {
        &self.certificate
    }

    /// The authority's certificate, in PEM, as a bundle holds it.
    pub fn certificate_pem(&self) -> &str {
        &self.pem
    }

    /// A certificate for `name`, a DNS name or an IP address, valid around
    /// `now`, for a server.
    pub fn issue(&self, name: &str, now: SystemTime) -> io::Result<Leaf> {
        let mut params = params(now)?;
        params.subject_alt_names = CertificateParams::new([name.to_owned()])
            .map_err(invalid)?
            .subject_alt_names;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.key_identifier_method = KeyIdMethod::PreSpecified(self.leaf.identifier());
        let certificate = params
            .signed_by(&self.leaf, &self.issuer)
            .map_err(invalid)?;
        Ok(Leaf {
            certificate: certificate.der().clone(),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf.pkcs8()?)),
        })
    }
}

/// The parameters every certificate shares: its dates around `now`, and a
/// random serial number.
fn params(now: SystemTime) -> io::Result<CertificateParams> {
    let mut serial = [0; 16];
    graviola::random::fill(&mut serial).map_err(crypto)?;
    // A serial number is positive.
    serial[0] &= 0x7f;
    let mut params = CertificateParams::default();
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.not_before = date(now.checked_sub(BEFORE).unwrap_or(UNIX_EPOCH))?;
    params.not_after = date(now + AFTER)?;
    Ok(params)
}

fn date(time: SystemTime) -> io::Result<OffsetDateTime> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    OffsetDateTime::from_unix_timestamp(seconds as i64).map_err(invalid)
}

/// A P-256 key, as `rcgen` signs with it.
struct Key {
    key: EcdsaKey<P256>,
    public: [u8; 65],
}

impl Key {
    fn generate() -> io::Result<Key> {
        let key = EcdsaKey::<P256>::generate().map_err(crypto)?;
        let public = key.private_key.public_key_uncompressed();
        Ok(Key { key, public })
    }

    /// RFC 7093's first method: the first 160 bits of the SHA-256 of the
    /// key's SubjectPublicKeyInfo.
    fn identifier(&self) -> Vec<u8> {
        let mut hash = Sha256Context::new();
        hash.update(&self.subject_public_key_info());
        hash.finish()[..20].to_vec()
    }

    fn pkcs8(&self) -> io::Result<Vec<u8>> {
        let mut buffer = [0; 256];
        let der = self.key.to_pkcs8_der(&mut buffer).map_err(crypto)?;
        Ok(der.to_vec())
    }
}

impl PublicKeyData for Key {
    fn der_bytes(&self) -> &[u8] {
        &self.public
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

impl SigningKey for Key {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        let mut signature = [0; 80];
        let signed = self.key.sign_asn1::<Sha256>(&[message], &mut signature);
        Ok(signed.map_err(|_| rcgen::Error::RemoteKeyError)?.to_vec())
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

fn crypto(error: graviola::Error) -> io::Error {
    io::Error::other(format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::RootCertStore;
    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{ServerName, UnixTime};
    use rustls::sign::CertifiedKey;
    use std::sync::Arc;

    /// Whether rustls' webpki verifier, trusting only `authority`, accepts
    /// `leaf` for `name` at `now`.
    fn verifies(authority: &Authority, leaf: &Leaf, name: &str, now: SystemTime) -> bool {
        let mut roots = RootCertStore::empty();
        roots.add(authority.certificate().clone()).unwrap();
        let provider = Arc::new(rustls_graviola::default_provider());
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .unwrap();
        let name = ServerName::try_from(name.to_owned()).unwrap();
        verifier
            .verify_server_cert(
                &leaf.certificate,
                &[],
                &name,
                &[],
                UnixTime::since_unix_epoch(now.duration_since(UNIX_EPOCH).unwrap()),
            )
            .is_ok()
    }

    #[test]
    fn a_leaf_verifies_against_only_its_authority_and_only_for_its_name() {
        let now = SystemTime::now();
        let authority = Authority::generate(now).unwrap();
        let leaf = authority.issue("api.example.com", now).unwrap();
        assert!(verifies(&authority, &leaf, "api.example.com", now));
        assert!(!verifies(&authority, &leaf, "other.example.com", now));
        assert!(!verifies(&authority, &leaf, "example.com", now));

        let another = Authority::generate(now).unwrap();
        let theirs = another.issue("api.example.com", now).unwrap();
        assert!(!verifies(&authority, &theirs, "api.example.com", now));
        assert_ne!(authority.certificate(), another.certificate());
    }

    #[test]
    fn a_leaf_verifies_for_an_address() {
        let now = SystemTime::now();
        let authority = Authority::generate(now).unwrap();
        let leaf = authority.issue("192.0.2.7", now).unwrap();
        assert!(verifies(&authority, &leaf, "192.0.2.7", now));
        assert!(!verifies(&authority, &leaf, "192.0.2.8", now));
    }

    #[test]
    fn a_leaf_is_valid_only_around_when_it_was_issued() {
        let now = SystemTime::now();
        let authority = Authority::generate(now).unwrap();
        let leaf = authority.issue("api.example.com", now).unwrap();
        let hour = Duration::from_secs(60 * 60);
        assert!(verifies(&authority, &leaf, "api.example.com", now - hour));
        assert!(!verifies(
            &authority,
            &leaf,
            "api.example.com",
            now - BEFORE - hour
        ));
        assert!(!verifies(
            &authority,
            &leaf,
            "api.example.com",
            now + AFTER + hour
        ));
    }

    #[test]
    fn a_leaf_key_matches_its_certificate() {
        let now = SystemTime::now();
        let authority = Authority::generate(now).unwrap();
        let leaf = authority.issue("api.example.com", now).unwrap();
        let provider = rustls_graviola::default_provider();
        let signer = provider.key_provider.load_private_key(leaf.key).unwrap();
        let certified = CertifiedKey::new(vec![leaf.certificate], signer);
        certified.keys_match().unwrap();
    }

    /// Set in the process that [`a_confined_network_process_issues_a_leaf`]
    /// starts, which confines itself.
    const CONFINED_VARIABLE: &str = "AP_AUTHORITY_CONFINED";
    const CONFINED_TEST: &str =
        "network::authority::tests::a_confined_network_process_issues_a_leaf";
    /// What the confined process prints once its leaf verifies.
    const VERIFIED: &str = "a leaf verified in a confined process";
    const GETRANDOM: std::ffi::c_long = 318;

    unsafe extern "C" {
        fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
    }

    /// The network process makes its authority before it confines itself,
    /// and issues leaves after, which draws on the random source. This
    /// starts this test again in a process of its own, which makes an
    /// authority, confines itself as the network process does when it may
    /// connect, and then issues a leaf and verifies it.
    #[test]
    fn a_confined_network_process_issues_a_leaf() {
        if std::env::var_os(CONFINED_VARIABLE).is_some() {
            let now = SystemTime::now();
            let authority = Authority::generate(now).unwrap();
            // Standard input is the link, which the filter lets it read.
            vmm::confine::confine_network_connecting(0).unwrap();
            let leaf = authority.issue("api.example.com", now).unwrap();
            assert!(verifies(&authority, &leaf, "api.example.com", now));
            // glibc can answer from the vDSO without a system call, but
            // musl and older glibc make the call, so this makes it too.
            let mut bytes = [0u8; 16];
            // SAFETY: getrandom writes at most `bytes.len()` bytes to it.
            let drawn = unsafe { syscall(GETRANDOM, bytes.as_mut_ptr(), bytes.len(), 0) };
            assert_eq!(drawn, bytes.len() as std::ffi::c_long);
            println!("{VERIFIED}");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([CONFINED_TEST, "--exact", "--test-threads=1", "--nocapture"])
            .env(CONFINED_VARIABLE, "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(VERIFIED),
            "{:?}\n{stdout}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_pem_holds_the_certificate() {
        let authority = Authority::generate(SystemTime::now()).unwrap();
        let pem = authority.certificate_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(pem.trim_end().ends_with("-----END CERTIFICATE-----"));
        assert!(pem.is_ascii() && !pem.contains('\0'));
    }
}
