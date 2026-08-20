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
}
