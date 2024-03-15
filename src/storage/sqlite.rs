// Copyright 2024 Simo Sorce
// See LICENSE.txt file for terms

use rusqlite::{params, Connection, Rows, ToSql, Transaction};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::super::error;
use super::super::interface;
use super::super::object;
use super::super::{err_not_found, err_rv};

use super::Storage;

use error::{KError, KResult};
use interface::*;
use object::Object;

/* causes .or to fail to find which impl to use
impl From<rusqlite::Error> for KError {
    fn from(error: rusqlite::Error) -> Self {
        KError::RvError(error::CkRvError { rv: CKR_DEVICE_MEMORY })
    }
}
*/

fn bad_code<T>(_error: T) -> KError {
    KError::RvError(error::CkRvError {
        rv: CKR_GENERAL_ERROR,
    })
}

fn bad_storage<T>(_error: T) -> KError {
    KError::RvError(error::CkRvError {
        rv: CKR_DEVICE_MEMORY,
    })
}

const IS_DB_INITIALIZED: &str =
    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='objects'";
const DROP_DB_TABLE: &str = "DROP TABLE objects";
const CREATE_DB_TABLE: &str = "CREATE TABLE objects (id int NOT NULL, attr int NOT NULL, val blob, enc int, UNIQUE (id, attr))";

/* search by filter constants */
const SEARCH_ALL: &str = "SELECT * FROM objects";
const SEARCH_NEST: &str = " WHERE id IN ( ";
const SEARCH_OBJ_ID: &str = "SELECT id FROM objects WHERE attr = ? AND val = ?";
const SEARCH_CONCAT: &str = " INTERSECT ";
const SEARCH_CLOSE: &str = " )";
const SEARCH_ORDER: &str = " ORDER by id";

const SEARCH_BY_SINGLE_ATTR: &str = "SELECT * FROM objects WHERE id IN (SELECT id FROM objects WHERE attr = ? AND val = ?)";
const UPDATE_ATTR: &str = "INSERT OR REPLACE INTO objects VALUES (?, ?, ?, ?)";
const DELETE_OBJ: &str = "DELETE FROM objects WHERE id = ?";
const MAX_ID: &str = "SELECT IFNULL(MAX(id), 0) FROM objects";

#[derive(Debug)]
pub struct SqliteStorage {
    filename: String,
    conn: Arc<Mutex<Connection>>,
    cache: HashMap<String, Object>,
}

impl SqliteStorage {
    fn is_initialized(&self) -> KResult<()> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row_and_then(IS_DB_INITIALIZED, [], |row| row.get(0))
            .map_err(bad_storage)?;
        match result {
            1 => Ok(()),
            0 => err_rv!(CKR_CRYPTOKI_NOT_INITIALIZED),
            _ => err_rv!(CKR_DEVICE_MEMORY),
        }
    }

    fn db_reset(&mut self) -> KResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let mut tx = conn.transaction().map_err(bad_storage)?;
        tx.set_drop_behavior(rusqlite::DropBehavior::Rollback);
        /* the drop can fail when files are empty (new) */
        let _ = tx.execute(DROP_DB_TABLE, params![]);
        tx.execute(CREATE_DB_TABLE, params![])
            .map_err(bad_storage)?;
        tx.commit().map_err(bad_storage)
    }

    fn rows_to_objects(mut rows: Rows) -> KResult<Vec<Object>> {
        let mut objid = 0;
        let mut objects = Vec::<Object>::new();
        while let Some(row) = rows.next().map_err(bad_storage)? {
            let id: i32 = row.get(0).map_err(bad_storage)?;
            let atype: CK_ULONG = row.get(1).map_err(bad_storage)?;
            let value = row
                .get_ref(2)
                .map_err(bad_storage)?
                .as_blob()
                .map_err(bad_code)?;
            /* TODO: enc */
            if objid != id {
                objid = id;
                objects.push(Object::new());
            }
            if let Some(obj) = objects.last_mut() {
                let ck_attr = CK_ATTRIBUTE {
                    type_: atype,
                    pValue: value.as_ptr() as *mut _,
                    ulValueLen: value.len() as CK_ULONG,
                };
                /* makes a copy of the blob */
                let attr = ck_attr.to_attribute()?;
                obj.set_attr(attr)?;
            } else {
                return err_rv!(CKR_GENERAL_ERROR);
            }
        }
        Ok(objects)
    }

    fn search_by_unique_id(conn: &Connection, uid: &String) -> KResult<Object> {
        let mut stmt = conn.prepare(SEARCH_BY_SINGLE_ATTR).map_err(bad_code)?;
        let rows = stmt
            .query(params![CKA_UNIQUE_ID, uid.as_bytes()])
            .map_err(bad_code)?;
        let mut objects = Self::rows_to_objects(rows)?;
        match objects.len() {
            0 => err_not_found!(uid.clone()),
            1 => Ok(objects.pop().unwrap()),
            _ => err_rv!(CKR_GENERAL_ERROR),
        }
    }

    fn search_with_filter(
        conn: &Connection,
        template: &[CK_ATTRIBUTE],
    ) -> KResult<Vec<Object>> {
        let mut search_query = String::from(SEARCH_ALL);
        let mut subqcount = 0;
        let mut search_params =
            Vec::<&dyn ToSql>::with_capacity(template.len() * 2);
        let mut params_holder =
            Vec::<(CK_ULONG, &[u8])>::with_capacity(template.len());
        for attr in template {
            /* add subqueries */
            if subqcount == 0 {
                search_query.push_str(SEARCH_NEST);
            } else {
                search_query.push_str(SEARCH_CONCAT);
            }
            search_query.push_str(SEARCH_OBJ_ID);
            /* add parameters */
            params_holder.push((attr.type_, unsafe {
                /* template is guaranteed to stay around
                 * for the life of the function so it is
                 * safe enough */
                std::slice::from_raw_parts(
                    attr.pValue as *const u8,
                    attr.ulValueLen as usize,
                )
            }));
            subqcount += 1;
        }
        if subqcount > 0 {
            /* reformat parameters for query */
            for p in params_holder.iter() {
                search_params.push(&p.0 as &dyn ToSql);
                search_params.push(&p.1 as &dyn ToSql);
            }
            search_query.push_str(SEARCH_CLOSE);
        }
        /* finally make sure results return ordered by id,
         * this simplifies conversion to actual Objects */
        search_query.push_str(SEARCH_ORDER);

        let mut stmt = conn.prepare(&search_query).map_err(bad_code)?;
        let rows = stmt.query(search_params.as_slice()).map_err(bad_code)?;
        Ok(Self::rows_to_objects(rows)?)
    }

    fn store_object(
        tx: &mut Transaction,
        uid: &String,
        obj: &Object,
    ) -> KResult<()> {
        let objid = match Self::delete_object(tx, uid)? {
            0 => {
                /* find new id to use for new object */
                let mut maxid = 0;
                let mut stmt = tx.prepare(MAX_ID).map_err(bad_code)?;
                let mut rows = stmt.query([]).map_err(bad_code)?;
                while let Some(row) = rows.next().map_err(bad_storage)? {
                    maxid = row.get(0).map_err(bad_storage)?;
                }
                maxid + 1
            }
            x => x,
        };
        let mut stmt = tx.prepare(UPDATE_ATTR).map_err(bad_storage)?;
        for a in obj.get_attributes() {
            let _ = stmt
                .execute(params![objid, a.get_type(), a.get_value(), 0])
                .map_err(bad_storage)?;
        }
        Ok(())
    }

    fn delete_object(tx: &mut Transaction, uid: &String) -> KResult<i32> {
        let mut stmt = tx.prepare(SEARCH_OBJ_ID).map_err(bad_storage)?;
        let objid = match stmt
            .query_row(params![CKA_UNIQUE_ID, uid.as_bytes()], |row| row.get(0))
        {
            Ok(r) => r,
            Err(e) => match e {
                rusqlite::Error::QueryReturnedNoRows => 0,
                _ => return err_rv!(CKR_DEVICE_MEMORY),
            },
        };
        /* remove old object */
        if objid != 0 {
            stmt = tx.prepare(DELETE_OBJ).map_err(bad_code)?;
            stmt.execute(params![objid]).map_err(bad_storage)?;
        }
        Ok(objid)
    }

    fn cache_obj(&mut self, uid: &String, mut obj: Object) {
        /* when inserting in cache we must clear the modified flag */
        obj.reset_modified();
        /* maintain handle/session when replacing */
        if let Some((uid_, old)) = self.cache.remove_entry(uid) {
            /* FIXME: bug on old.is_modified() ? */
            obj.set_handle(old.get_handle());
            obj.set_session(old.get_session());
            self.cache.insert(uid_, obj);
        } else {
            self.cache.insert(uid.clone(), obj);
        }
    }
}

impl Storage for SqliteStorage {
    fn open(&mut self, filename: &String) -> KResult<()> {
        self.filename = filename.clone();
        self.conn = match Connection::open(&self.filename) {
            Ok(c) => Arc::new(Mutex::from(c)),
            Err(_) => return err_rv!(CKR_TOKEN_NOT_PRESENT),
        };
        self.cache.clear();
        self.is_initialized()
    }
    fn reinit(&mut self) -> KResult<()> {
        self.db_reset()
    }
    fn flush(&mut self) -> KResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let mut tx = conn.transaction().map_err(bad_storage)?;
        tx.set_drop_behavior(rusqlite::DropBehavior::Rollback);
        for (uid, obj) in &mut self.cache {
            if obj.is_modified() {
                obj.reset_modified();
                if obj.is_token() {
                    Self::store_object(&mut tx, uid, obj)?;
                }
            }
        }
        tx.commit().map_err(bad_storage)
    }
    fn fetch_by_uid(&mut self, uid: &String) -> KResult<&Object> {
        let is_token = match self.get_cached_by_uid(uid) {
            Ok(obj) => obj.is_token(),
            _ => true,
        };
        if is_token {
            let conn = self.conn.lock().unwrap();
            let obj = Self::search_by_unique_id(&conn, uid)?;
            drop(conn);
            self.cache_obj(uid, obj);
        }
        self.get_cached_by_uid(uid)
    }
    fn get_cached_by_uid(&self, uid: &String) -> KResult<&Object> {
        if let Some(o) = self.cache.get(uid) {
            return Ok(o);
        }
        err_not_found!(uid.clone())
    }
    fn get_cached_by_uid_mut(&mut self, uid: &String) -> KResult<&mut Object> {
        if !self.cache.contains_key(uid) {
            let conn = self.conn.lock().unwrap();
            let obj = Self::search_by_unique_id(&conn, uid)?;
            drop(conn);
            self.cache_obj(uid, obj);
        }
        if let Some(o) = self.cache.get_mut(uid) {
            return Ok(o);
        }
        err_not_found!(uid.clone())
    }
    fn store(&mut self, uid: &String, obj: Object) -> KResult<()> {
        if obj.is_token() {
            let mut conn = self.conn.lock().unwrap();
            let mut tx = conn.transaction().map_err(bad_storage)?;
            tx.set_drop_behavior(rusqlite::DropBehavior::Rollback);
            Self::store_object(&mut tx, uid, &obj)?;
            tx.commit().map_err(bad_storage)?;
        }
        self.cache_obj(uid, obj);
        Ok(())
    }
    fn get_all_cached(&self) -> Vec<&Object> {
        let mut result = Vec::<&Object>::with_capacity(self.cache.len());
        for (_, o) in self.cache.iter() {
            result.push(o);
        }
        result
    }
    fn search(&mut self, template: &[CK_ATTRIBUTE]) -> KResult<Vec<&Object>> {
        let conn = self.conn.lock().unwrap();
        let mut objects = Self::search_with_filter(&conn, template)?;
        drop(conn);
        for obj in objects.drain(..) {
            /* if uid is not available we can only skip */
            let uid = match obj.get_attr_as_string(CKA_UNIQUE_ID) {
                Ok(u) => u,
                Err(_) => continue,
            };
            self.cache_obj(&uid, obj);
        }
        let mut result = Vec::<&Object>::new();
        for (_, o) in self.cache.iter() {
            if o.match_template(template) {
                result.push(o);
            }
        }
        Ok(result)
    }
    fn remove_by_uid(&mut self, uid: &String) -> KResult<()> {
        let is_token = match self.get_cached_by_uid(uid) {
            Ok(obj) => obj.is_token(),
            _ => true,
        };
        self.cache.remove(uid);
        if is_token {
            let mut conn = self.conn.lock().unwrap();
            let mut tx = conn.transaction().map_err(bad_storage)?;
            tx.set_drop_behavior(rusqlite::DropBehavior::Rollback);
            Self::delete_object(&mut tx, &uid)?;
            tx.commit().map_err(bad_storage)?;
        }
        Ok(())
    }
}

pub fn sqlite() -> Box<dyn Storage> {
    Box::new(SqliteStorage {
        filename: String::from(""),
        conn: Arc::new(Mutex::from(Connection::open_in_memory().unwrap())),
        cache: HashMap::new(),
    })
}
