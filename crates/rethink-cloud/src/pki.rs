//! In-process CA, server certificates, and CSR signing.
//!
//! Replaces the `openssl` CLI (`req`, `x509 -req`). The OpenSSL *library* is
//! used so RSA issuance works without a `PATH` binary. Fields follow upstream
//! `util/pki.ts`: CN-only subjects, SHA-256, CA basic constraints with no
//! keyUsage, and a server leaf with serverAuth. The bridge makes its own CSRs
//! (`rethink_bridge::pair`).

use anyhow::{bail, Context, Result};
use openssl::asn1::Asn1Integer;
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage,
    SubjectAlternativeName, SubjectKeyIdentifier,
};
use openssl::x509::{X509NameBuilder, X509};
use std::fs;
use std::io::Write;
use std::path::Path;

pub const CA_COMMON_NAME: &str = "Rethink CA";
const DAYS: u32 = 3650;
/// Decimal 100, which is what `openssl x509 -set_serial 0100` produced (hex 0x64).
const DEVICE_SERIAL_HEX: &str = "64";

#[derive(Clone)]
pub struct Certificate {
    pub key_pem: String,
    pub cert_pem: String,
}

pub struct Issuer {
    key: PKey<Private>,
    cert: X509,
}

fn name_cn(cn: &str) -> Result<openssl::x509::X509Name> {
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, cn)?;
    Ok(name.build())
}

fn random_serial() -> Result<Asn1Integer> {
    let mut bn = BigNum::new()?;
    // Top bit set so the 20-byte serial is never zero.
    bn.rand(160, MsbOption::ONE, false)?;
    Ok(bn.to_asn1_integer()?)
}

fn pem_string(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Self-signed RSA-4096 CA. Not a TLS server certificate.
pub fn create_self_signed_ca() -> Result<Certificate> {
    let rsa = Rsa::generate(4096).context("generate CA key")?;
    let key = PKey::from_rsa(rsa)?;
    let subject = name_cn(CA_COMMON_NAME)?;
    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    builder.set_serial_number(random_serial()?.as_ref())?;
    builder.set_subject_name(&subject)?;
    builder.set_issuer_name(&subject)?;
    builder.set_pubkey(&key)?;
    builder.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    builder.set_not_after(Asn1Time::days_from_now(DAYS)?.as_ref())?;
    builder.append_extension(BasicConstraints::new().critical().ca().build()?)?;
    let ski = {
        let ctx = builder.x509v3_context(None, None);
        SubjectKeyIdentifier::new().build(&ctx)?
    };
    builder.append_extension(ski)?;
    // Self-signed, so this repeats the subject key identifier added just above.
    let aki = {
        let ctx = builder.x509v3_context(None, None);
        AuthorityKeyIdentifier::new().keyid(true).build(&ctx)?
    };
    builder.append_extension(aki)?;
    builder.sign(&key, MessageDigest::sha256())?;
    let cert = builder.build();
    Ok(Certificate {
        key_pem: pem_string(&key.private_key_to_pem_pkcs8()?),
        cert_pem: pem_string(&cert.to_pem()?),
    })
}

pub fn load_issuer(ca: &Certificate) -> Result<Issuer> {
    let cert = X509::from_pem(ca.cert_pem.as_bytes()).context("parse CA certificate")?;
    let key = PKey::private_key_from_pem(ca.key_pem.as_bytes()).context("parse CA key")?;
    let cert_pub = cert.public_key()?.public_key_to_der()?;
    let key_pub = key.public_key_to_der()?;
    if cert_pub != key_pub {
        bail!("the CA private key does not belong to the CA certificate");
    }
    Ok(Issuer { key, cert })
}

/// RSA-2048 TLS leaf for `hostname`, signed by `issuer`.
pub fn create_server_certificate(hostname: &str, issuer: &Issuer) -> Result<Certificate> {
    let rsa = Rsa::generate(2048).context("generate server key")?;
    let key = PKey::from_rsa(rsa)?;
    let subject = name_cn(hostname)?;
    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    builder.set_serial_number(random_serial()?.as_ref())?;
    builder.set_subject_name(&subject)?;
    builder.set_issuer_name(issuer.cert.subject_name())?;
    builder.set_pubkey(&key)?;
    builder.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    builder.set_not_after(Asn1Time::days_from_now(DAYS)?.as_ref())?;
    builder.append_extension(BasicConstraints::new().critical().build()?)?;
    builder.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;
    builder.append_extension(ExtendedKeyUsage::new().server_auth().build()?)?;
    let san = {
        let ctx = builder.x509v3_context(Some(&issuer.cert), None);
        SubjectAlternativeName::new().dns(hostname).build(&ctx)?
    };
    builder.append_extension(san)?;
    let ski = {
        let ctx = builder.x509v3_context(Some(&issuer.cert), None);
        SubjectKeyIdentifier::new().build(&ctx)?
    };
    builder.append_extension(ski)?;
    let aki = {
        let ctx = builder.x509v3_context(Some(&issuer.cert), None);
        AuthorityKeyIdentifier::new().keyid(true).build(&ctx)?
    };
    builder.append_extension(aki)?;
    builder.sign(&issuer.key, MessageDigest::sha256())?;
    let cert = builder.build();
    Ok(Certificate {
        key_pem: pem_string(&key.private_key_to_pem_pkcs8()?),
        cert_pem: pem_string(&cert.to_pem()?),
    })
}

/// Sign a CSR. Only the subject and public key are taken from the request.
pub fn sign_certificate_request(csr_pem: &str, issuer: &Issuer, serial_hex: &str) -> Result<String> {
    let csr = openssl::x509::X509Req::from_pem(csr_pem.as_bytes()).context("parse CSR")?;
    let pubkey = csr.public_key().context("CSR public key")?;
    let bn = BigNum::from_hex_str(serial_hex).context("serial")?;
    let serial = bn.to_asn1_integer()?;
    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    builder.set_serial_number(serial.as_ref())?;
    builder.set_subject_name(csr.subject_name())?;
    builder.set_issuer_name(issuer.cert.subject_name())?;
    builder.set_pubkey(&pubkey)?;
    builder.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    builder.set_not_after(Asn1Time::days_from_now(DAYS)?.as_ref())?;
    builder.sign(&issuer.key, MessageDigest::sha256())?;
    let _ = bn;
    Ok(pem_string(&builder.build().to_pem()?))
}

pub fn sign_device_csr(csr_pem: &str, issuer: &Issuer) -> Result<String> {
    sign_certificate_request(csr_pem, issuer, DEVICE_SERIAL_HEX)
}

/// Read a custom root PEM. A missing file or a non-certificate fails.
pub fn load_custom_root(path: &Path) -> Result<String> {
    let pem = fs::read_to_string(path)
        .with_context(|| format!("read custom root {}", path.display()))?;
    X509::from_pem(pem.as_bytes())
        .with_context(|| format!("{} is not a certificate", path.display()))?;
    Ok(pem)
}

/// Like `fs::write`, but a file it creates is readable by its owner only.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(contents.as_bytes())
}

pub fn load_or_create(key_path: &Path, cert_path: &Path) -> Result<Certificate> {
    let key_exists = key_path.exists();
    let cert_exists = cert_path.exists();
    if !key_exists && !cert_exists {
        tracing::info!("Creating a new key/certificate for the CA");
        eprintln!("[status] Creating a new key/certificate for the CA");
        let ca = create_self_signed_ca()?;
        if let Some(parent) = key_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = cert_path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private(key_path, &ca.key_pem)?;
        fs::write(cert_path, &ca.cert_pem)?;
        return Ok(ca);
    }
    if !key_exists {
        bail!("{} is missing, but {} is not", key_path.display(), cert_path.display());
    }
    if !cert_exists {
        bail!("{} is missing, but {} is not", cert_path.display(), key_path.display());
    }
    let ca = Certificate {
        key_pem: fs::read_to_string(key_path).context("read ca key")?,
        cert_pem: fs::read_to_string(cert_path).context("read ca cert")?,
    };
    load_issuer(&ca).with_context(|| {
        format!(
            "{} and {} are not a usable CA",
            cert_path.display(),
            key_path.display()
        )
    })?;
    Ok(ca)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn subject_cn(pem: &str) -> String {
        let cert = X509::from_pem(pem.as_bytes()).unwrap();
        cert.subject_name()
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .unwrap()
            .data()
            .as_utf8()
            .unwrap()
            .to_string()
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rethink-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A P-256 request, like the ones appliances send.
    fn device_csr(common_name: &str) -> String {
        let group = openssl::ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = PKey::from_ec_key(openssl::ec::EcKey::generate(&group).unwrap()).unwrap();
        let subject = name_cn(common_name).unwrap();
        let mut req = openssl::x509::X509ReqBuilder::new().unwrap();
        req.set_subject_name(&subject).unwrap();
        req.set_pubkey(&key).unwrap();
        req.sign(&key, MessageDigest::sha256()).unwrap();
        pem_string(&req.build().to_pem().unwrap())
    }

    #[test]
    fn ca_server_csr_and_sign_do_not_need_the_openssl_cli() {
        // PATH without a binary: a leftover `Command::new("openssl")` would fail.
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", "/nonexistent-rethink-path");

        let ca = create_self_signed_ca().expect("CA");
        assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert_eq!(subject_cn(&ca.cert_pem), CA_COMMON_NAME);
        let issuer = load_issuer(&ca).unwrap();
        let ca_x509 = X509::from_pem(ca.cert_pem.as_bytes()).unwrap();
        assert!(ca_x509
            .verify(&ca_x509.public_key().unwrap())
            .unwrap());

        let server = create_server_certificate("lg.example", &issuer).expect("server");
        assert_eq!(subject_cn(&server.cert_pem), "lg.example");
        let server_x509 = X509::from_pem(server.cert_pem.as_bytes()).unwrap();
        assert!(server_x509.verify(&ca_x509.public_key().unwrap()).unwrap());
        let sans = server_x509
            .subject_alt_names()
            .expect("SAN")
            .iter()
            .filter_map(|n| n.dnsname())
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert!(sans.iter().any(|s| s == "lg.example"));
        assert_ne!(subject_cn(&server.cert_pem), CA_COMMON_NAME);

        let signed = sign_device_csr(&device_csr("device.local"), &issuer).unwrap();
        let signed_x509 = X509::from_pem(signed.as_bytes()).unwrap();
        assert_eq!(subject_cn(&signed), "device.local");
        assert!(signed_x509.verify(&ca_x509.public_key().unwrap()).unwrap());
        let serial = signed_x509.serial_number().to_bn().unwrap();
        assert_eq!(serial.to_dec_str().unwrap().to_string(), "100");

        if let Some(path) = saved {
            std::env::set_var("PATH", path);
        }
    }

    #[test]
    fn ca_and_server_certificates_identify_the_signing_key() {
        let ca = create_self_signed_ca().unwrap();
        let ca_x509 = X509::from_pem(ca.cert_pem.as_bytes()).unwrap();
        let ca_ski = ca_x509.subject_key_id().expect("CA SKI");
        // Self-signed: the authority is the CA's own key.
        assert_eq!(
            ca_x509.authority_key_id().expect("CA AKI").as_slice(),
            ca_ski.as_slice()
        );

        let issuer = load_issuer(&ca).unwrap();
        let server = create_server_certificate("rethink.lan", &issuer).unwrap();
        let server_x509 = X509::from_pem(server.cert_pem.as_bytes()).unwrap();
        assert_eq!(
            server_x509.authority_key_id().expect("leaf AKI").as_slice(),
            ca_ski.as_slice()
        );
        assert_ne!(
            server_x509.subject_key_id().expect("leaf SKI").as_slice(),
            ca_ski.as_slice()
        );
    }

    #[test]
    fn a_created_ca_key_is_readable_by_its_owner_only() {
        let dir = scratch_dir("pki-ca");
        let key = dir.join("data/ca.key");
        let cert = dir.join("data/ca.cert");
        let ca = load_or_create(&key, &cert).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The next start loads the same CA instead of minting another.
        let again = load_or_create(&key, &cert).unwrap();
        assert_eq!(again.key_pem, ca.key_pem);
        assert_eq!(again.cert_pem, ca.cert_pem);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_root_loader_rejects_a_non_certificate() {
        let dir = scratch_dir("pki");
        let bad = dir.join("not-a-cert.pem");
        std::fs::write(&bad, "this is not a certificate\n").unwrap();
        let err = load_custom_root(&bad).unwrap_err().to_string();
        assert!(err.contains("is not a certificate"), "{err}");

        let ca = create_self_signed_ca().unwrap();
        let good = dir.join("root.pem");
        std::fs::write(&good, &ca.cert_pem).unwrap();
        let loaded = load_custom_root(&good).unwrap();
        assert_eq!(loaded, ca.cert_pem);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
