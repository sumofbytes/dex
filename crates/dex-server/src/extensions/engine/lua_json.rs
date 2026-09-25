//! JSON<->Lua conversion and stringification helpers shared by the host
//! API and the call runner: type names, safe stringify, tool-result
//! flattening, and the boundary-exact `lua_to_json` / `json_to_lua` pair.

use mlua::{Error as LuaError, Lua, MultiValue, Value};
use serde_json::{Map, Value as Json};

pub(super) fn lua_type_name(value: &Value) -> &'static str {
    match value {
        Value::Nil => "nil",
        Value::Boolean(_) => "boolean",
        Value::LightUserData(_) => "userdata",
        Value::Integer(_) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Table(_) => "table",
        Value::Function(_) => "function",
        Value::Thread(_) => "thread",
        Value::UserData(_) => "userdata",
        Value::Error(_) => "error",
        Value::Other(_) => "other",
    }
}

pub(super) fn stringify_json(value: &Value) -> Result<Json, String> {
    match value {
        Value::String(s) => Ok(Json::String(
            s.to_str().map_err(|e| e.to_string())?.to_string(),
        )),
        Value::Integer(i) => Ok(Json::Number((*i).into())),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .ok_or_else(|| "non-finite number".to_string()),
        Value::Boolean(b) => Ok(Json::Bool(*b)),
        Value::Nil => Ok(Json::Null),
        Value::Table(_) => lua_to_json(value.clone()),
        other => Err(format!("cannot pass {} to host", lua_type_name(other))),
    }
}

pub(super) fn lua_value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s
            .to_str()
            .map(|b| b.to_string())
            .unwrap_or_else(|_| "<invalid utf-8>".to_string()),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".to_string(),
        Value::Table(_) => match lua_to_json(value.clone()) {
            Ok(json) => json.to_string(),
            Err(_) => "<unserializable>".to_string(),
        },
        other => format!("<{}>", lua_type_name(other)),
    }
}

/// Tool results are display strings: scalars stringify, tables serialize as
/// JSON, anything else (functions, threads) is a registration-time-shaped
/// error at call time.
pub(super) fn stringify_tool_result(
    returned: MultiValue,
    ev_id: &str,
    tool: &str,
) -> Result<String, String> {
    let first = returned.into_iter().next().unwrap_or(Value::Nil);
    match first {
        Value::String(s) => s.to_str().map(|b| b.to_string()).map_err(|e| e.to_string()),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Boolean(b) => Ok(b.to_string()),
        Value::Nil => Ok(String::new()),
        Value::Table(_) => serde_json::to_string(
            &lua_to_json(first).map_err(|e| format!("extension '{ev_id}' tool '{tool}': {e}"))?,
        )
        .map_err(|e| e.to_string()),
        other => Err(format!(
            "extension '{ev_id}' tool '{tool}' returned {}",
            lua_type_name(&other)
        )),
    }
}

/// Host JSON → Lua: objects to string-keyed tables, arrays to 1-based
/// tables. Non-string object keys stringify (JSON round-trips are
/// string-keyed at the top; nested numbers become `"1"`-style keys).
pub(super) fn json_to_lua(lua: &Lua, value: &Json) -> Result<Value, LuaError> {
    match value {
        Json::Null => Ok(Value::Nil),
        Json::Bool(b) => Ok(Value::Boolean(*b)),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Ok(Value::Nil)
            }
        }
        Json::String(s) => Ok(Value::String(lua.create_string(s)?)),
        Json::Array(items) => {
            let table = lua.create_table()?;
            for (index, item) in items.iter().enumerate() {
                table.set(index + 1, json_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(table))
        }
        Json::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.set(key.clone(), json_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// Lua → host JSON. Tables are arrays iff non-empty with exactly the integer
/// keys `1..=n`; empty tables become `{}` (args-shaped; a nested empty array
/// degrades — hooks mutating exotic shapes re-check the host side).
/// Functions, threads, and userdata cannot cross and are an error, never a
/// silent drop.
pub(super) fn lua_to_json(value: Value) -> Result<Json, String> {
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(b) => Ok(Json::Bool(b)),
        Value::Integer(i) => Ok(Json::Number(i.into())),
        Value::Number(n) => serde_json::Number::from_f64(n)
            .map(Json::Number)
            .ok_or_else(|| "non-finite number cannot cross to host".to_string()),
        Value::String(s) => Ok(Json::String(
            s.to_str().map_err(|e| e.to_string())?.to_string(),
        )),
        Value::Table(t) => {
            let mut pairs: Vec<(Value, Value)> = t
                .pairs()
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            if pairs.is_empty() {
                return Ok(Json::Object(Map::new()));
            }
            pairs.sort_by(|a, b| {
                let ai = a.0.as_integer();
                let bi = b.0.as_integer();
                ai.cmp(&bi)
            });
            let is_array = pairs
                .iter()
                .enumerate()
                .all(|(index, (key, _))| key.as_integer() == Some(index as i64 + 1));
            if is_array {
                let mut items = Vec::with_capacity(pairs.len());
                for (_, item) in pairs {
                    items.push(lua_to_json(item)?);
                }
                Ok(Json::Array(items))
            } else {
                let mut map = Map::with_capacity(pairs.len());
                for (key, item) in pairs {
                    let key = match key {
                        Value::String(s) => s.to_str().map_err(|e| e.to_string())?.to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => n.to_string(),
                        Value::Boolean(b) => b.to_string(),
                        other => {
                            return Err(format!(
                                "cannot use {} as an object key",
                                lua_type_name(&other)
                            ));
                        }
                    };
                    map.insert(key, lua_to_json(item)?);
                }
                Ok(Json::Object(map))
            }
        }
        other => Err(format!("cannot pass {} to host", lua_type_name(&other))),
    }
}
