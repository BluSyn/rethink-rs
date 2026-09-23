//! One TLS leaf per requested server name, signed by our CA.
//!
//! Names that are not hostnames, and anything past [`MAX_CERTIFICATES`],
//! fall back to the certificate for `default_hostname`.

use crate::pki::{create_server_certificate, Certificate, Issuer};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;

const MAX_CERTIFICATES: usize = 64;

pub fn is_plausible_hostname(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    let mut labels = name.split('.');
    let mut any = false;
    for label in labels.by_ref() {
        any = true;
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return false;
        }
        let ok = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
        if !bytes.iter().all(|c| ok(*c)) {
            return false;
        }
        if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
            return false;
        }
    }
    any
}

pub struct CertificateIssuer {
    issuer: Issuer,
    default_hostname: String,
    issued: Mutex<HashMap<String, Certificate>>,
}

impl CertificateIssuer {
    pub fn new(issuer: Issuer, default_hostname: impl Into<String>) -> Result<Self> {
        let default_hostname = default_hostname.into();
        let mut issued = HashMap::new();
        let leaf = create_server_certificate(&default_hostname, &issuer)?;
        issued.insert(default_hostname.clone(), leaf);
        Ok(Self {
            issuer,
            default_hostname,
            issued: Mutex::new(issued),
        })
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.issuer.cert_pem
    }

    pub fn default_hostname(&self) -> &str {
        &self.default_hostname
    }

    /// Certificate (key PEM, cert PEM) presented for this TLS server name.
    pub fn select(&self, servername: &str) -> (String, String) {
        let name = if servername == self.default_hostname || !is_plausible_hostname(servername) {
            self.default_hostname.clone()
        } else {
            servername.to_string()
        };
        let mut map = self.issued.lock().unwrap_or_else(|e| e.into_inner());
        if name != self.default_hostname && !map.contains_key(&name) && map.len() >= MAX_CERTIFICATES
        {
            let fallback = map.get(&self.default_hostname).expect("default leaf");
            return (fallback.key_pem.clone(), fallback.cert_pem.clone());
        }
        if !map.contains_key(&name) {
            if let Ok(leaf) = create_server_certificate(&name, &self.issuer) {
                map.insert(name.clone(), leaf);
            }
        }
        let cert = map
            .get(&name)
            .or_else(|| map.get(&self.default_hostname))
            .expect("default leaf");
        (cert.key_pem.clone(), cert.cert_pem.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::{create_self_signed_ca, load_issuer};
    use openssl::nid::Nid;
    use openssl::x509::X509;

    fn cn(pem: &str) -> String {
        X509::from_pem(pem.as_bytes())
            .unwrap()
            .subject_name()
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .unwrap()
            .data()
            .as_utf8()
            .unwrap()
            .to_string()
    }

    #[test]
    fn selector_returns_the_cert_for_each_hostname() {
        let ca = create_self_signed_ca().unwrap();
        let issuer = load_issuer(&ca).unwrap();
        let sni = CertificateIssuer::new(issuer, "rethink.lan").unwrap();
        let (_, a) = sni.select("oven.lan");
        let (_, b) = sni.select("washer.lan");
        let (_, again) = sni.select("oven.lan");
        assert_eq!(cn(&a), "oven.lan");
        assert_eq!(cn(&b), "washer.lan");
        assert_eq!(a, again, "the same name reuses one certificate");
        assert_ne!(a, b);
        let (_, fallback) = sni.select("not a hostname");
        assert_eq!(cn(&fallback), "rethink.lan");
        let (_, empty) = sni.select("");
        assert_eq!(cn(&empty), "rethink.lan");
    }
}
