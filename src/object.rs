// Copyright 2023 Simo Sorce
// See LICENSE.txt file for terms

use super::attribute;
use super::error;
use super::interface;
use super::{err_not_found, err_rv};
use attribute::{AttrType, Attribute};
use error::{KError, KResult};
use interface::*;

use uuid::Uuid;

macro_rules! create_bool_checker {
    (make $name:ident; from $id:expr; def $def:expr) => {
        pub fn $name(&self) -> bool {
            for a in &self.attributes {
                if a.get_type() == $id {
                    return a.to_bool().unwrap_or($def);
                }
            }
            $def
        }
    };
}

macro_rules! attr_as_type {
    (make $name:ident; with $r:ty; $atype:ident; via $conv:ident) => {
        pub fn $name(&self, t: CK_ULONG) -> KResult<$r> {
            for attr in &self.attributes {
                if attr.get_type() == t {
                    if attr.get_attrtype() != AttrType::$atype {
                        return err_rv!(CKR_ATTRIBUTE_TYPE_INVALID);
                    }
                    return attr.$conv();
                }
            }
            err_not_found!(t.to_string())
        }
    };
}

static SENSITIVE_CKK_RSA: [CK_ULONG; 6] = [
    CKA_PRIVATE_EXPONENT,
    CKA_PRIME_1,
    CKA_PRIME_2,
    CKA_EXPONENT_1,
    CKA_EXPONENT_2,
    CKA_COEFFICIENT,
];

static SENSITIVE_CKK_EC: [CK_ULONG; 1] = [CKA_VALUE];

static SENSITIVE_CKK_DH: [CK_ULONG; 2] = [CKA_VALUE, CKA_VALUE_BITS];

static SENSITIVE_CKK_DSA: [CK_ULONG; 1] = [CKA_VALUE];

static SENSITIVE_CKK_GENERIC_SECRET: [CK_ULONG; 2] = [CKA_VALUE, CKA_VALUE_LEN];

static SENSITIVE: [(CK_ULONG, &[CK_ULONG]); 8] = [
    (CKK_RSA, &SENSITIVE_CKK_RSA),
    (CKK_EC, &SENSITIVE_CKK_EC),
    (CKK_EC_EDWARDS, &SENSITIVE_CKK_EC),
    (CKK_EC_MONTGOMERY, &SENSITIVE_CKK_EC),
    (CKK_DH, &SENSITIVE_CKK_DH),
    (CKK_X9_42_DH, &SENSITIVE_CKK_DH),
    (CKK_DSA, &SENSITIVE_CKK_DSA),
    (CKK_GENERIC_SECRET, &SENSITIVE_CKK_GENERIC_SECRET),
];

#[derive(Debug, Clone)]
pub struct Object {
    handle: CK_OBJECT_HANDLE,
    attributes: Vec<Attribute>,
}

impl Object {
    pub fn new(handle: CK_ULONG) -> Object {
        Object {
            handle: handle,
            attributes: Vec::new(),
        }
    }

    pub fn get_handle(&self) -> CK_OBJECT_HANDLE {
        self.handle
    }

    create_bool_checker! {make is_token; from CKA_TOKEN; def false}
    create_bool_checker! {make is_private; from CKA_PRIVATE; def true}
    create_bool_checker! {make is_sensitive; from CKA_SENSITIVE; def true}
    create_bool_checker! {make is_modifiable; from CKA_MODIFIABLE; def true}
    create_bool_checker! {make is_destroyable; from CKA_DESTROYABLE; def false}
    create_bool_checker! {make is_extractable; from CKA_EXTRACTABLE; def false}

    pub fn set_attr(&mut self, a: Attribute) -> KResult<()> {
        let mut idx = self.attributes.len();
        for (i, elem) in self.attributes.iter().enumerate() {
            if a.get_type() == elem.get_type() {
                idx = i;
                break;
            }
        }
        if idx < self.attributes.len() {
            self.attributes[idx] = a;
        } else {
            self.attributes.push(a);
        }
        Ok(())
    }

    pub fn get_attributes(&self) -> &Vec<Attribute> {
        return &self.attributes;
    }

    attr_as_type! {make get_attr_as_bool; with bool; BoolType; via to_bool}
    attr_as_type! {make get_attr_as_ulong; with CK_ULONG; NumType; via to_ulong}
    attr_as_type! {make get_attr_as_string; with String; StringType; via to_string}
    attr_as_type! {make get_attr_as_bytes; with &Vec<u8>; BytesType; via to_bytes}

    pub fn match_template(&self, template: &[CK_ATTRIBUTE]) -> bool {
        for ck_attr in template.iter() {
            let mut found = false;
            for attr in &self.attributes {
                found = attr.match_ck_attr(ck_attr);
                if found {
                    break;
                }
            }
            if !found {
                return false;
            }
        }
        true
    }

    fn private_key_type(&self) -> Option<CK_ULONG> {
        let mut class: CK_ULONG = CK_UNAVAILABLE_INFORMATION;
        let mut key_type: CK_ULONG = CK_UNAVAILABLE_INFORMATION;
        for attr in &self.attributes {
            if attr.get_type() == CKA_CLASS {
                class = attr.to_ulong().unwrap_or(CK_UNAVAILABLE_INFORMATION);
                continue;
            }
            if attr.get_type() == CKA_KEY_TYPE {
                key_type =
                    attr.to_ulong().unwrap_or(CK_UNAVAILABLE_INFORMATION);
            }
        }
        if class == CKO_PRIVATE_KEY || class == CKO_SECRET_KEY {
            return Some(key_type);
        }
        None
    }

    fn needs_sensitivity_check(&self) -> Option<&[CK_ULONG]> {
        let kt = self.private_key_type()?;
        for tuple in SENSITIVE {
            if tuple.0 == kt {
                return Some(tuple.1);
            }
        }
        None
    }

    fn is_sensitive_attr(&self, id: CK_ULONG, sense: &[CK_ULONG]) -> bool {
        if !sense.contains(&id) {
            return false;
        }
        if self.is_sensitive() {
            return true;
        }
        if !self.is_extractable() {
            return true;
        }
        false
    }

    pub fn fill_template(&self, template: &mut [CK_ATTRIBUTE]) -> KResult<()> {
        let sense = self.needs_sensitivity_check();
        let mut rv = CKR_OK;
        for elem in template.iter_mut() {
            if let Some(s) = sense {
                if self.is_sensitive_attr(elem.type_, s) {
                    elem.ulValueLen = CK_UNAVAILABLE_INFORMATION;
                    rv = CKR_ATTRIBUTE_SENSITIVE;
                    continue;
                }
            }
            let mut found = false;
            for attr in &self.attributes {
                if attr.get_type() == elem.type_ {
                    found = true;
                    if elem.pValue.is_null() {
                        elem.ulValueLen = attr.get_value().len() as CK_ULONG;
                        break;
                    }
                    let val = attr.get_value();
                    if (elem.ulValueLen as usize) < val.len() {
                        elem.ulValueLen = CK_UNAVAILABLE_INFORMATION;
                        rv = CKR_BUFFER_TOO_SMALL;
                        break;
                    }
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            val.as_ptr(),
                            elem.pValue as *mut _,
                            val.len(),
                        );
                    }
                    elem.ulValueLen = val.len() as CK_ULONG;
                    break;
                }
            }
            if !found {
                elem.ulValueLen = CK_UNAVAILABLE_INFORMATION;
                rv = CKR_ATTRIBUTE_TYPE_INVALID;
            }
        }
        if rv == CKR_OK {
            return Ok(());
        }
        err_rv!(rv)
    }
}

#[derive(Debug, Clone, Copy)]
struct ObjectAttr {
    id: CK_ULONG,
    required: bool,
    present: bool,
    default: bool, /* only for bool values */
    default_value: bool,
}

macro_rules! req_element {
    ($id:expr) => {
        ObjectAttr {
            id: $id,
            required: true,
            present: false,
            default: false,
            default_value: false,
        }
    };
    ($id:expr; def $def:expr) => {
        ObjectAttr {
            id: $id,
            required: true,
            present: false,
            default: true,
            default_value: $def,
        }
    };
}
macro_rules! opt_element {
    ($id:expr) => {
        ObjectAttr {
            id: $id,
            required: false,
            present: false,
            default: false,
            default_value: false,
        }
    };
    ($id:expr; def $def:expr) => {
        ObjectAttr {
            id: $id,
            required: false,
            present: false,
            default: true,
            default_value: $def,
        }
    };
}

fn basic_object_attrs_checks(
    obj: &mut Object,
    cattrs: &mut Vec<ObjectAttr>,
) -> CK_RV {
    for attr in &obj.attributes {
        let typ = attr.get_type();
        let mut valid = false;
        for elem in cattrs.iter_mut() {
            if typ == elem.id {
                if elem.present {
                    /* duplicate */
                    return CKR_TEMPLATE_INCONSISTENT;
                }
                valid = true;
                elem.present = true;
                break;
            }
        }
        if !valid {
            return CKR_ATTRIBUTE_VALUE_INVALID;
        }
    }

    for elem in cattrs.iter_mut() {
        if !elem.present && elem.default {
            obj.attributes
                .push(attribute::from_bool(elem.id, elem.default_value));
            elem.present = true;
        }
        if elem.required && !elem.present {
            return CKR_TEMPLATE_INCOMPLETE;
        }
    }

    CKR_OK
}

static COMMON_OBJ_ATTRS: [ObjectAttr; 1] = [req_element!(CKA_CLASS)];

static COMMON_STORAGE_ATTRS: [ObjectAttr; 7] = [
    opt_element!(CKA_TOKEN; def false),
    opt_element!(CKA_PRIVATE; def false),
    opt_element!(CKA_MODIFIABLE; def true),
    opt_element!(CKA_LABEL),
    opt_element!(CKA_COPYABLE; def true),
    opt_element!(CKA_DESTROYABLE; def true),
    req_element!(CKA_UNIQUE_ID),
];

static DATA_OBJECT_ATTRS: [ObjectAttr; 3] = [
    req_element!(CKA_APPLICATION),
    opt_element!(CKA_OBJECT_ID),
    req_element!(CKA_VALUE),
];

fn create_data_object(mut obj: Object) -> KResult<Object> {
    let mut cattrs = Vec::<ObjectAttr>::with_capacity(
        COMMON_OBJ_ATTRS.len()
            + COMMON_STORAGE_ATTRS.len()
            + DATA_OBJECT_ATTRS.len(),
    );
    cattrs.extend(COMMON_OBJ_ATTRS);
    cattrs.extend(COMMON_STORAGE_ATTRS);
    cattrs.extend(DATA_OBJECT_ATTRS);

    let ret = basic_object_attrs_checks(&mut obj, &mut cattrs);
    if ret != CKR_OK {
        return err_rv!(ret);
    }
    Ok(obj)
}

pub fn create(handle: CK_ULONG, template: &[CK_ATTRIBUTE]) -> KResult<Object> {
    let mut obj = Object {
        handle: handle,
        attributes: Vec::new(),
    };

    let uuid = Uuid::new_v4().to_string();
    obj.attributes
        .push(attribute::from_string(CKA_UNIQUE_ID, uuid));

    for ck_attr in template.iter() {
        obj.attributes.push(ck_attr.to_attribute()?);
    }

    let class = match obj.get_attr_as_ulong(CKA_CLASS) {
        Ok(c) => c,
        Err(_) => return err_rv!(CKR_TEMPLATE_INCOMPLETE),
    };
    match class {
        CKO_DATA => create_data_object(obj),
        CKO_CERTIFICATE => err_rv!(CKR_FUNCTION_FAILED),
        CKO_PUBLIC_KEY => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_PRIVATE_KEY => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_SECRET_KEY => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_HW_FEATURE => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_DOMAIN_PARAMETERS => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_MECHANISM => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_OTP_KEY => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_PROFILE => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        CKO_VENDOR_DEFINED => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
        _ => err_rv!(CKR_ATTRIBUTE_VALUE_INVALID),
    }
}
