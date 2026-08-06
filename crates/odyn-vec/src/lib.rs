//! The one unsafe call in the workspace: statically registering sqlite-vec
//! with SQLite before any connection opens. odyn-core forbids unsafe code and
//! `forbid` cannot be locally allowed, so the registration lives here behind a
//! safe wrapper.

use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

/// SQLite's auto-extension entry-point shape, as rusqlite's ffi expects it.
type ExtensionInit = unsafe extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *mut c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> c_int;

static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();

/// Registers sqlite-vec for every SQLite connection this process opens from
/// now on. Idempotent: the first outcome is returned on every later call.
pub fn register() -> Result<(), String> {
    REGISTERED
        .get_or_init(|| {
            let rc = unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                    *const (),
                    ExtensionInit,
                >(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )))
            };
            if rc == rusqlite::ffi::SQLITE_OK {
                Ok(())
            } else {
                Err(format!("sqlite-vec registration failed with code {rc}"))
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_serves_a_vec_virtual_table() {
        register().expect("register");
        register().expect("idempotent");
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE VIRTUAL TABLE v USING vec0(embedding float[4])")
            .expect("create vec table");
        conn.execute(
            "INSERT INTO v(rowid, embedding) VALUES (1, ?)",
            [&[0.0f32, 1.0, 0.0, 0.0]
                .iter()
                .flat_map(|f| f.to_le_bytes())
                .collect::<Vec<u8>>()],
        )
        .expect("insert");
        let distance: f64 = conn
            .query_row(
                "SELECT distance FROM v WHERE embedding MATCH ? ORDER BY distance LIMIT 1",
                [&[0.0f32, 1.0, 0.0, 0.0]
                    .iter()
                    .flat_map(|f| f.to_le_bytes())
                    .collect::<Vec<u8>>()],
                |row| row.get(0),
            )
            .expect("knn");
        assert!(
            distance.abs() < 1e-6,
            "identical vectors, distance {distance}"
        );
    }
}
