//! 上传业务类型常量 (等价 app/core/upload_types.py)

pub const UPLOAD_TYPE_LOGISTICS: &str = "物流";
pub const UPLOAD_TYPE_WAREHOUSE: &str = "仓库";
pub const VALID_UPLOAD_TYPES: [&str; 2] = [UPLOAD_TYPE_LOGISTICS, UPLOAD_TYPE_WAREHOUSE];
pub const DEFAULT_UPLOAD_TYPE: &str = UPLOAD_TYPE_LOGISTICS;

/// sorted(VALID_UPLOAD_TYPES) 以 ", " 连接 (按码点排序: 仓库 < 物流)
pub fn valid_upload_types_sorted() -> String {
    let mut v = VALID_UPLOAD_TYPES.to_vec();
    v.sort();
    v.join(", ")
}

/// DOC_TYPE_TO_BUSINESS_TYPE.get(doc_type, settings.YONYOU_BUSINESS_TYPE)
pub fn business_type_for(doc_type: Option<&str>) -> String {
    match doc_type {
        Some("销售") => "yonbip-scm-scmsa".to_string(),
        Some("转库") | Some("其他") => "yonbip-scm-stock".to_string(),
        _ => crate::config::get().yonyou_business_type.clone(),
    }
}

pub const VALID_DOC_TYPES: [&str; 3] = ["销售", "转库", "其他"];
