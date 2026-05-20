use anyhow::{Context, Result};
use pyo3::prelude::*;
use serde_json::Value;

const MODULE_SRC: &str = include_str!("../ytm-api/main.py");

const SITE_PACKAGES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/ytm-api/.venv/lib/python3.12/site-packages"
);

fn call<F, T>(f: F) -> Result<T>
where
    F: FnOnce(Python, &Bound<PyModule>) -> PyResult<T>,
{
    Python::with_gil(|py| {
        let sys = py.import_bound("sys")?;
        sys.getattr("path")?
            .call_method1("insert", (0, SITE_PACKAGES))?;

        let module = PyModule::from_code_bound(py, MODULE_SRC, "main.py", "main")?;
        f(py, &module)
    })
    .map_err(|e| anyhow::anyhow!("{e}"))
}

pub fn get_library_playlists() -> Result<Vec<Value>> {
    let json = call(|_py, m| {
        m.getattr("get_library_playlists")?
            .call0()?
            .extract::<String>()
    })?;
    serde_json::from_str(&json).context("parse get_library_playlists response")
}

pub fn search_playlists(query: &str) -> Result<Vec<Value>> {
    let json = call(|_py, m| {
        m.getattr("search_playlists")?
            .call1((query,))?
            .extract::<String>()
    })?;
    serde_json::from_str(&json).context("parse search_playlists response")
}
