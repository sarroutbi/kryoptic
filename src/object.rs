// Copyright 2023 Simo Sorce
// See LICENSE.txt file for terms

use std::collections::HashMap;

use super::interface;
use super::attribute;
use super::error;
use interface::*;
use attribute::{Attribute, AttrType};
use error::{KResult, KError};
use super::{err_rv, err_not_found};

use serde::{Serialize, Deserialize};
use serde_json::{Map, Value};

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
    }
}

macro_rules! attr_as_type {
    (make $name:ident; with $r:ty; $atype:ident; via $conv:ident) => {
        pub fn $name(&self, t: CK_ULONG) -> KResult<$r> {
            for attr in &self.attributes {
                if attr.get_type() == t {
                    if attr.get_attrtype() != AttrType::$atype {
                        return err_rv!(CKR_ATTRIBUTE_TYPE_INVALID);
                    }
                    return attr.$conv()
                }
            }
            err_not_found!(t.to_string())
        }
    }
}

static SENSITIVE_CKK_RSA: [CK_ULONG; 6] = [
    CKA_PRIVATE_EXPONENT,
    CKA_PRIME_1,
    CKA_PRIME_2,
    CKA_EXPONENT_1,
    CKA_EXPONENT_2,
    CKA_COEFFICIENT,
];

static SENSITIVE_CKK_EC: [CK_ULONG; 1] = [
    CKA_VALUE,
];

static SENSITIVE_CKK_DH: [CK_ULONG; 2] = [
    CKA_VALUE,
    CKA_VALUE_BITS,
];

static SENSITIVE_CKK_DSA: [CK_ULONG; 1] = [
    CKA_VALUE,
];

static SENSITIVE_CKK_GENERIC_SECRET: [CK_ULONG; 2] = [
    CKA_VALUE,
    CKA_VALUE_LEN,
];

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
    attributes: Vec<Attribute>
}

impl Object {
    pub fn new() -> Object {
        Object {
            handle: 0,
            attributes: Vec::new(),
        }
    }

    pub fn get_handle(&self) -> CK_OBJECT_HANDLE {
        self.handle
    }

    create_bool_checker!{make is_token; from CKA_TOKEN; def false}
    create_bool_checker!{make is_private; from CKA_PRIVATE; def true}
    create_bool_checker!{make is_sensitive; from CKA_SENSITIVE; def true}
    create_bool_checker!{make is_modifiable; from CKA_MODIFIABLE; def true}
    create_bool_checker!{make is_destroyable; from CKA_DESTROYABLE; def false}
    create_bool_checker!{make is_extractable; from CKA_EXTRACTABLE; def false}

    fn set_attr(&mut self, a: Attribute) -> KResult<()> {
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

    attr_as_type!{make get_attr_as_bool; with bool; BoolType; via to_bool}
    attr_as_type!{make get_attr_as_ulong; with CK_ULONG; NumType; via to_ulong}
    attr_as_type!{make get_attr_as_string; with String; StringType; via to_string}
    attr_as_type!{make get_attr_as_bytes; with &Vec<u8>; BytesType; via to_bytes}

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
                key_type = attr.to_ulong().unwrap_or(CK_UNAVAILABLE_INFORMATION);
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
                        std::ptr::copy_nonoverlapping(val.as_ptr(), elem.pValue as *mut _, val.len());
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

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonObject {
    handle: CK_OBJECT_HANDLE,
    attributes: Map<String, Value>
}

pub fn objects_to_json(objs: &HashMap<CK_ULONG, Object>) -> Vec<JsonObject> {
    let mut jobjs = Vec::new();

    for (h, o) in objs {
        let mut jo = JsonObject {
            handle: *h,
            attributes: Map::new()
        };
        for a in &o.attributes {
            jo.attributes.insert(a.name(), a.json_value());
        }
        jobjs.push(jo);
    }
    jobjs
}

pub fn json_to_objects(jobjs: &Vec<JsonObject>) -> HashMap<CK_ULONG, Object> {
    let mut objs = HashMap::new();

    for jo in jobjs {
        let mut o = Object {
            handle: jo.handle,
            attributes: Vec::new(),
        };
        for jk in jo.attributes.keys() {
            if let Ok(a) = attribute::from_value(jk.clone(), jo.attributes.get(jk).unwrap()) {
                o.attributes.push(a);
            }
        }
        objs.insert(o.handle, o);
    }
    objs
}
