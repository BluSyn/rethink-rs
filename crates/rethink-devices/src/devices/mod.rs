//! Per-model device handler modules.
//!
//! To add a new device:
//! 1. Create `devices/<module>.rs` with `create(...)` and `#[cfg(test)]` tests
//! 2. Add **one line** to the `register_devices!` table below
//!
//! Module names intentionally track LG `modelId` strings (double underscores, etc.).

#![allow(non_snake_case)]

pub mod washer_ctrl;

/// Declare modules + `modelId` factories from one table.
///
/// Syntax: `module_name => { "MODEL_ID" | "ALIAS" }`.
/// `t2` is ThinQ2 (`T2Factory`); `t1` is ThinQ1 (`T1Factory`).
macro_rules! register_devices {
    (
        t2 { $( $t2mod:ident => { $($t2id:literal)|+ } ),+ $(,)? }
        t1 { $( $t1mod:ident => { $($t1id:literal)|+ } ),+ $(,)? }
    ) => {
        $( pub mod $t2mod; )+
        $( pub mod $t1mod; )+

        pub fn t2_factory(model_id: &str) -> Option<crate::device_trait::T2Factory> {
            match model_id {
                $( $($t2id)|+ => Some($t2mod::create), )+
                _ => None,
            }
        }

        pub fn t1_factory(model_id: &str) -> Option<crate::device_trait::T1Factory> {
            match model_id {
                $( $($t1id)|+ => Some($t1mod::create), )+
                _ => None,
            }
        }

        pub fn all_t2_model_ids() -> Vec<&'static str> {
            vec![ $( $($t2id,)+ )+ ]
        }

        pub fn all_t1_model_ids() -> Vec<&'static str> {
            vec![ $( $($t1id,)+ )+ ]
        }
    };
}

register_devices! {
    t2 {
        pot_056905_ww => { "POT_056905_WW" },
        rac_056905_ww => { "RAC_056905_WW" | "RAC_0B0001_WW" },
        cst_570004_ww => { "CST_570004_WW" },
        win_056905_ww => { "WIN_056905_WW" },
        dev_2ref11eida__4 => { "2REF11EIDA__4" },
        dev_2ref11ebivpc4 => { "2REF11EBIVPC4" },
        dev_2ref12eii_p_2 => { "2REF12EII_P_2" },
        dev_2res1ve61nfa2 => { "2RES1VE61NFA2" },
        dev_2reb1glvb1__2 => { "2REB1GLVB1__2" },
        dev_2rebglub_2p__ => { "2REBGLUB_2P__" },
        dev_2res1ve600fwc => { "2RES1VE600FWC" },
        y_v8_y___w_b32qeuk => { "Y_V8_Y___W.B32QEUK" },
        f_v8_y___w_b_2qeuk => {
            "F_V7_Y___W.B_2QEUK" | "F_V8_Y___W.B_2QEUK" | "F_V__Y___W.B_2QEUK" | "F_V7_Y___W.B__QEUK"
        },
        y_v8_f___w_b_2qeuk => { "Y_V8_F___W.B_2QEUK" },
        vcdwl2qeuk => { "VCDWL2QEUK" },
        f_v__f___w_b_1qeuk => { "F_V__F___W.B_1QEUK" | "F_VA_F___W.B__QEUK" },
        f_vb_f___w_b_2qeuk => { "F_VB_F___W.B_2QEUK" },
        f_c__y___w_a__qeuk => { "F_C__Y___W.A__QEUK" },
        h11 => { "H11" },
        t1789efh_f => { "T1789EFH_F" },
        t17a1efhu_f => { "T17A1EFHU_F" },
        rv13u6am8w_d_us_wifi => { "RV13U6AM8W_D_US_WIFI" },
        f3l2cyu__ => { "F3L2CYU__" },
        f3l7cyk5w_us_wifi => { "F3L7CYK5W_US_WIFI" },
        rv13b6bsd_d_us_wifi => { "RV13B6BSD_D_US_WIFI" },
        rv13b6es_d_us_wifi => { "RV13B6ES_D_US_WIFI" },
        rh10v9_ch => { "RH10V9_CH" },
        wtl_fxu_bdv_na_01 => { "WTL_FXU_BDV_NA_01" },
        dhum_056905_ww => { "DHUM_056905_WW" },
        dhum_231006_ww => { "DHUM_231006_WW" },
        hum_056905_ww => { "HUM_056905_WW" },
        studio_hood => { "STUDIO_HOOD" },
        hwwa9k_f2 => { "HWWA9K_F2" },
        dev_1wpu4cigcr__2 => { "1WPU4CIGCR__2" },
        pac_910604_ww => { "PAC_910604_WW" },
        st_b_e4h01y_apl => { "ST_B_E4H01Y_APL" },
        rh90v9_ww => { "RH90V9_WW" },
        wfv474pgv => { "WFV474PGV" },
    }
    t1 {
        wtdn3 => { "WTDN3" },
    }
}
