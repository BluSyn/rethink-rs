//! modelId → handler factory registry.
//!
//! Adding a new device is two steps:
//! 1. Implement `devices/<module>.rs` (`create` + `#[cfg(test)]` tests in that file)
//! 2. Add one line to the `register_devices!` table in `devices/mod.rs`

use crate::device_trait::{T1Factory, T2Factory};

pub use crate::devices::{all_t1_model_ids, all_t2_model_ids, t1_factory, t2_factory};

/// Lookup used by ha_bridge / cloud.
pub fn lookup_t2(model_id: &str) -> Option<T2Factory> {
    t2_factory(model_id)
}

pub fn lookup_t1(model_id: &str) -> Option<T1Factory> {
    t1_factory(model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen pre-Rust-rewrite ThinQ2 modelIds (including aliases).
    const PRE_REWRITE_T2: &[&str] = &[
        "POT_056905_WW",
        "RAC_056905_WW",
        "RAC_0B0001_WW",
        "CST_570004_WW",
        "WIN_056905_WW",
        "2REF11EIDA__4",
        "2REF11EBIVPC4",
        "2RES1VE61NFA2",
        "2REB1GLVB1__2",
        "2RES1VE600FWC",
        "Y_V8_Y___W.B32QEUK",
        "F_V7_Y___W.B_2QEUK",
        "F_V8_Y___W.B_2QEUK",
        "Y_V8_F___W.B_2QEUK",
        "F_V__Y___W.B_2QEUK",
        "VCDWL2QEUK",
        "F_V__F___W.B_1QEUK",
        "F_VA_F___W.B__QEUK",
        "F_VB_F___W.B_2QEUK",
        "T1789EFH_F",
        "RV13U6AM8W_D_US_WIFI",
        "F3L2CYU__",
        "RV13B6BSD_D_US_WIFI",
        "RH10V9_CH",
        "WTL_FXU_BDV_NA_01",
        "DHUM_056905_WW",
    ];

    const PRE_REWRITE_T1: &[&str] = &["WTDN3"];

    #[test]
    fn all_t2_models_have_factories() {
        for id in all_t2_model_ids() {
            assert!(t2_factory(id).is_some(), "missing t2 factory for {id}");
        }
    }

    #[test]
    fn all_t1_models_have_factories() {
        for id in all_t1_model_ids() {
            assert!(t1_factory(id).is_some(), "missing t1 factory for {id}");
        }
    }

    #[test]
    fn every_pre_rewrite_model_has_factory() {
        for id in PRE_REWRITE_T2 {
            assert!(
                t2_factory(id).is_some(),
                "missing ThinQ2 factory for pre-rewrite modelId {id}"
            );
        }
        for id in PRE_REWRITE_T1 {
            assert!(
                t1_factory(id).is_some(),
                "missing ThinQ1 factory for pre-rewrite modelId {id}"
            );
        }
        let t2: std::collections::HashSet<_> = all_t2_model_ids().into_iter().collect();
        let t1: std::collections::HashSet<_> = all_t1_model_ids().into_iter().collect();
        for id in PRE_REWRITE_T2 {
            assert!(t2.contains(id), "all_t2_model_ids missing {id}");
        }
        for id in PRE_REWRITE_T1 {
            assert!(t1.contains(id), "all_t1_model_ids missing {id}");
        }
    }

    #[test]
    fn aliases_share_handler_with_canonical() {
        let rac = t2_factory("RAC_056905_WW").unwrap();
        let rac_alias = t2_factory("RAC_0B0001_WW").unwrap();
        let cst = t2_factory("CST_570004_WW").unwrap();
        assert!(std::ptr::fn_addr_eq(rac, rac_alias));
        assert!(
            !std::ptr::fn_addr_eq(rac, cst),
            "CST cassette is not a RAC wall-unit alias"
        );

        let f = t2_factory("F_V__F___W.B_1QEUK").unwrap();
        let f_va = t2_factory("F_VA_F___W.B__QEUK").unwrap();
        assert!(std::ptr::fn_addr_eq(f, f_va));

        let fv7 = t2_factory("F_V7_Y___W.B_2QEUK").unwrap();
        let fv7_alias = t2_factory("F_V7_Y___W.B__QEUK").unwrap();
        assert!(std::ptr::fn_addr_eq(fv7, fv7_alias));
    }

    /// Deterministic xorshift, so a failing frame reproduces.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    /// Truncated and malformed AABB frames for every common class/type, TLV-shaped frames, and
    /// random bytes.
    fn malformed_frames() -> Vec<Vec<u8>> {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut out = vec![vec![], vec![0xaa], vec![0xaa, 0xbb], vec![0xaa, 0x04, 0x00, 0xbb]];
        let classes = [0x10u8, 0x20, 0x30, 0x32, 0x33, 0x34, 0x40, 0x50, 0x60];
        let types = [
            0xebu8, 0xec, 0xe2, 0xd8, 0x0a, 0xcd, 0xbd, 0x72, 0x7f, 0x31, 0xc5, 0x10, 0xde, 0x41,
            0x88, 0x92, 0x00, 0x60, 0x1f, 0x43,
        ];
        for class in classes {
            for ty in types {
                for len in 0..120 {
                    let mut random = vec![class, ty];
                    random.extend(rng.bytes(len));
                    out.push(rethink_core::wrap_aabb(&random));
                    let mut zeros = vec![class, ty];
                    zeros.resize(len + 2, 0);
                    out.push(rethink_core::wrap_aabb(&zeros));
                }
            }
        }
        for len in 0..40 {
            for kind in [0x87u8, 0xa7] {
                let mut tlv = vec![0x01, 0x00, 0x04, 0x00, 0x00, 0x00, kind, 0x02, 0x01, 0x00, len as u8];
                tlv.extend(rng.bytes(len));
                tlv.extend([0x00, 0x00]);
                out.push(tlv.clone());
                tlv.truncate(tlv.len().saturating_sub(3));
                out.push(tlv);
            }
            let mut priv_resp = vec![0x02, 0x01, 0x04, 0x00, 0x00, 0x00, 0x87, 0xfd, 0x10, 0x00, 0x05, 0xfe];
            priv_resp.extend(rng.bytes(len));
            out.push(priv_resp);
            let mut priv_data = vec![0x01, 0xff, 0x04, 0x00, 0x00, 0x00, 0x87, 0xfd, 0x03, 0x00, len as u8];
            priv_data.extend(rng.bytes(len));
            out.push(priv_data);
        }
        for _ in 0..2000 {
            let len = (rng.next() % 200) as usize;
            out.push(rng.bytes(len));
        }
        out
    }

    /// A panic in a data handler would take down that appliance's connection task.
    #[test]
    fn no_handler_panics_on_malformed_frames() {
        use rethink_core::{MockHaConnection, MockThinq1Device, MockThinq2Device, Metadata};
        use rethink_core::{Thinq1Device, Thinq2Device};
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let frames = malformed_frames();
        let first_panic = |emit: &dyn Fn(&[u8])| {
            frames
                .iter()
                .find(|f| catch_unwind(AssertUnwindSafe(|| emit(f))).is_err())
                .map(|f| rethink_core::hex_encode(f))
        };
        let mut failures = Vec::new();
        for model in all_t2_model_ids() {
            let meta = Metadata::new(model, model, "1.0");
            let thinq = MockThinq2Device::new("id", meta.clone());
            let dev = t2_factory(model).unwrap()(MockHaConnection::new(), thinq.clone(), meta);
            dev.start();
            if let Some(frame) = first_panic(&|f| thinq.emit_data(f)) {
                failures.push(format!("{model}: {frame}"));
            }
        }
        for model in all_t1_model_ids() {
            let meta = Metadata::new(model, model, "1.0");
            let thinq = MockThinq1Device::new("id", meta.clone());
            let dev = t1_factory(model).unwrap()(MockHaConnection::new(), thinq.clone(), meta);
            dev.start();
            if let Some(frame) = first_panic(&|f| thinq.emit_data(f)) {
                failures.push(format!("ThinQ1 {model}: {frame}"));
            }
        }
        assert!(failures.is_empty(), "handlers panicked: {failures:#?}");
    }
}
