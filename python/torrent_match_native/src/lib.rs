use ptt_core::{clean_title_native, expand_number_range, languages_translation_table, parse_many, parse_title, parse_title_context, translate_langs_codes};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use serde_json::{Map, Value};

fn value_to_py(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    match value {
        Value::Null => Ok(py.None()),
        Value::Bool(b) => Ok((*b).into_pyobject(py)?.to_owned().into_any().unbind()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { Ok(i.into_pyobject(py)?.into_any().unbind()) }
            else if let Some(u) = n.as_u64() { Ok(u.into_pyobject(py)?.into_any().unbind()) }
            else if let Some(f) = n.as_f64() { Ok(f.into_pyobject(py)?.into_any().unbind()) }
            else { Ok(py.None()) }
        }
        Value::String(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items { list.append(value_to_py(py, item)?)?; }
            Ok(list.into_any().unbind())
        }
        Value::Object(map) => map_to_py(py, map),
    }
}
fn map_to_py(py: Python<'_>, map: &Map<String, Value>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for (k,v) in map { dict.set_item(k, value_to_py(py, v)?)?; }
    Ok(dict.into_any().unbind())
}
#[pyfunction]
#[pyo3(signature=(raw_title, translate_languages=false))]
fn ptt_parse_title(py: Python<'_>, raw_title: &str, translate_languages: bool) -> PyResult<Py<PyAny>> {
    if raw_title.is_empty() { return Err(PyValueError::new_err("raw_title must not be empty")); }
    let parsed = parse_title(raw_title, translate_languages).map_err(|e| PyValueError::new_err(e.to_string()))?;
    map_to_py(py, &parsed)
}
fn byte_to_char_index(text: &str, byte_index: usize) -> usize {
    let mut boundary = byte_index.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) { boundary -= 1; }
    text[..boundary].chars().count()
}
#[pyfunction]
fn ptt_parse_title_context(py: Python<'_>, raw_title: &str) -> PyResult<Py<PyAny>> {
    if raw_title.is_empty() { return Err(PyValueError::new_err("raw_title must not be empty")); }
    let context = parse_title_context(raw_title, false).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let output = PyDict::new(py);
    output.set_item("result", map_to_py(py, &context.result)?)?;
    output.set_item("working_title", &context.working_title)?;
    output.set_item("end_of_title", byte_to_char_index(&context.working_title, context.end_of_title))?;
    let matched = PyDict::new(py);
    for (name, info) in context.matched {
        let item = PyDict::new(py);
        item.set_item("raw_match", info.raw_match)?;
        item.set_item("match_index", byte_to_char_index(&context.working_title, info.match_index))?;
        matched.set_item(name, item)?;
    }
    output.set_item("matched", matched)?;
    Ok(output.into_any().unbind())
}
#[pyfunction]
#[pyo3(signature=(titles, translate_languages=false))]
fn ptt_parse_many(py: Python<'_>, titles: Vec<String>, translate_languages: bool) -> PyResult<Py<PyAny>> {
    if titles.iter().any(String::is_empty) { return Err(PyValueError::new_err("titles must not contain empty strings")); }
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    let parsed = parse_many(refs, translate_languages).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let list = PyList::empty(py);
    for item in parsed { list.append(map_to_py(py, &item)?)?; }
    Ok(list.into_any().unbind())
}
#[pyfunction]
fn ptt_clean_title(raw_title: &str) -> String { clean_title_native(raw_title) }
#[pyfunction]
fn ptt_translate_langs(langs: Vec<String>) -> Vec<String> { translate_langs_codes(&langs) }
#[pyfunction]
fn ptt_languages_translation_table(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for (k,v) in languages_translation_table() { dict.set_item(k,v)?; }
    Ok(dict.into_any().unbind())
}
#[pyfunction]
fn ptt_expand_number_range(input: &str) -> Option<Vec<i64>> { expand_number_range(input) }
#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(ptt_expand_number_range,m)?)?;
    m.add_function(wrap_pyfunction!(ptt_parse_title,m)?)?;
    m.add_function(wrap_pyfunction!(ptt_parse_title_context,m)?)?;
    m.add_function(wrap_pyfunction!(ptt_parse_many,m)?)?;
    m.add_function(wrap_pyfunction!(ptt_clean_title,m)?)?;
    m.add_function(wrap_pyfunction!(ptt_translate_langs,m)?)?;
    m.add_function(wrap_pyfunction!(ptt_languages_translation_table,m)?)?;
    Ok(())
}
