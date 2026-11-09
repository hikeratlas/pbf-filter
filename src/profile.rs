//! A tilemaker profile, as far as what it reads from a PBF: its `way_keys`
//! and `node_keys`, whether its `node_function` does anything, and its
//! `relation_scan_function`, which is run here in LuaJIT as tilemaker runs it.

use mlua::{Function, Lua, Table, Value};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// tilemaker's SignificantTags: `[~]key[=value]` filters, all accepting or
/// all rejecting (`~`). A declared but empty list matches nothing.
#[derive(Clone, Debug)]
pub struct TagFilter {
    accept: bool,
    filters: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// As declared, for recording in a filtered PBF.
    pub raw: Vec<String>,
}

impl TagFilter {
    pub fn parse(raw: &[String]) -> Result<TagFilter, String> {
        let mut filters = vec![];
        let mut accept = None;
        for r in raw {
            let (a, rest) = match r.strip_prefix('~') {
                Some(rest) => (false, rest),
                None => (true, r.as_str()),
            };
            if accept.is_some_and(|x| x != a) {
                return Err(format!("cannot mix reject and accept filters: {}, {r}", raw[0]));
            }
            accept = Some(a);
            filters.push(match rest.split_once('=') {
                Some((k, v)) => (k.as_bytes().to_vec(), Some(v.as_bytes().to_vec())),
                None => (rest.as_bytes().to_vec(), None),
            });
        }
        Ok(TagFilter { accept: accept.unwrap_or(true), filters, raw: raw.to_vec() })
    }

    /// Whether every tag set this filter matches, some filter in `others`
    /// matches too. Exact for accept lists; reject lists (~) only cover an
    /// identical one.
    pub fn covered_by(&self, others: &[&TagFilter]) -> bool {
        if !self.accept {
            return others.iter().any(|o| !o.accept && o.filters == self.filters);
        }
        self.filters.iter().all(|(k, v)| {
            others.iter().any(|o| {
                o.accept && o.filters.iter().any(|(ok, ov)| ok == k && (ov.is_none() || ov == v))
            })
        })
    }

    fn hit(&self, k: &[u8], v: &[u8]) -> bool {
        self.filters.iter().any(|(fk, fv)| fk == k && fv.as_deref().is_none_or(|fv| fv == v))
    }

    pub fn matches<'a>(&self, tags: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> bool {
        if self.filters.is_empty() {
            return false;
        }
        if self.accept {
            tags.into_iter().any(|(k, v)| self.hit(k, v))
        } else {
            // At least one tag no filter matches.
            tags.into_iter().any(|(k, v)| !self.hit(k, v))
        }
    }
}

/// What decides which ways and nodes a profile reads: pure data, shareable
/// across threads.
#[derive(Clone, Debug)]
pub struct Rules {
    pub name: String,
    /// None: no way_keys, so every way.
    pub way_keys: Option<TagFilter>,
    /// None: no node_keys.
    pub node_keys: Option<TagFilter>,
    /// Whether node_function (if any) does anything.
    pub node_function: bool,
}

impl Rules {
    /// What a filtered PBF records about the rules it was made for.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "way_keys": self.way_keys.as_ref().map(|f| f.raw.clone()),
            "node_keys": self.node_keys.as_ref().map(|f| f.raw.clone()),
            "node_function": self.node_function,
        })
    }

    pub fn from_json(v: &serde_json::Value) -> Result<Rules, String> {
        let keys = |k: &str| -> Result<Option<TagFilter>, String> {
            match &v[k] {
                serde_json::Value::Null => Ok(None),
                serde_json::Value::Array(a) => {
                    let raw: Vec<String> = a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect();
                    TagFilter::parse(&raw).map(Some)
                }
                _ => Err(format!("bad {k} in a filtered PBF's record")),
            }
        };
        Ok(Rules {
            name: v["name"].as_str().unwrap_or("").to_string(),
            way_keys: keys("way_keys")?,
            node_keys: keys("node_keys")?,
            node_function: v["node_function"].as_bool().unwrap_or(true),
        })
    }

    /// Whether these rules read no ways or nodes beyond what `old` (the
    /// rules a filtered PBF was made for) read; None if so, else why not.
    pub fn uncovered_by(&self, old: &[Rules]) -> Option<String> {
        // Ways: every way's tags matching ours must match one of theirs.
        match &self.way_keys {
            None if !old.iter().any(|o| o.way_keys.is_none()) => return Some(format!("{}: no way_keys (every way)", self.name)),
            Some(f) if !old.iter().any(|o| o.way_keys.is_none()) => {
                let theirs: Vec<&TagFilter> = old.iter().filter_map(|o| o.way_keys.as_ref()).collect();
                if !f.covered_by(&theirs) {
                    return Some(format!("{}: way_keys {:?} match ways the PBF may not have", self.name, f.raw));
                }
            }
            _ => {}
        }
        // Nodes node_function sees: tagged, and matching node_keys if any.
        if self.node_function {
            let all_tagged = old.iter().any(|o| o.node_function && o.node_keys.is_none());
            match &self.node_keys {
                None if !all_tagged => return Some(format!("{}: node_function with no node_keys (every tagged node)", self.name)),
                Some(f) if !all_tagged => {
                    let theirs: Vec<&TagFilter> = old.iter().filter(|o| o.node_function).filter_map(|o| o.node_keys.as_ref()).collect();
                    if !f.covered_by(&theirs) {
                        return Some(format!("{}: node_keys {:?} match nodes the PBF may not have", self.name, f.raw));
                    }
                }
                _ => {}
            }
        }
        None
    }

    pub fn keeps_way<'a>(&self, tags: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> bool {
        self.way_keys.as_ref().is_none_or(|f| f.matches(tags))
    }

    /// Whether node_function would see this node (tilemaker: a tagged node,
    /// matching node_keys if there are any).
    pub fn keeps_node<'a>(&self, tags: &[(&'a [u8], &'a [u8])]) -> bool {
        self.node_function && !tags.is_empty() && self.node_keys.as_ref().is_none_or(|f| f.matches(tags.iter().copied()))
    }
}

#[derive(Default)]
struct Scan {
    id: i64,
    tags: Vec<(String, String)>,
    accepted: bool,
}

pub struct Profile {
    pub rules: Rules,
    scan: Option<Function>,
    state: Rc<RefCell<Scan>>,
    _lua: Lua,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Profile, String> {
        let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let dir = path.parent().map(|d| d.canonicalize().unwrap_or(d.to_path_buf())).unwrap_or_default();
        // All the standard libraries, as tilemaker loads them (debug is used
        // below to find node_function's source lines).
        let lua = unsafe { Lua::unsafe_new_with(mlua::StdLib::ALL, mlua::LuaOptions::default()) };
        let err = |e: mlua::Error| format!("{name}: {e}");
        let state = Rc::new(RefCell::new(Scan::default()));
        {
            let g = lua.globals();
            // The modules a profile requires from its own directory; any that
            // won't load here (e.g. needing a database or a C module) are
            // stood in for by a table of no-ops: only the profile's
            // declarations and relation_scan_function matter.
            lua.load(format!(
                r#"package.path = {:?} .. "/?.lua;" .. package.path
                local real_require = require
                function require(name)
                  local ok, m = pcall(real_require, name)
                  if ok then return m end
                  return setmetatable({{}}, {{__index = function() return function() end end}})
                end"#,
                dir.display().to_string()
            ))
            .exec()
            .map_err(err)?;
            // What relation_scan_function may call: tilemaker's API there.
            let s = state.clone();
            g.set("Find", lua.create_function(move |_, k: String| {
                Ok(s.borrow().tags.iter().find(|(tk, _)| *tk == k).map(|(_, v)| v.clone()).unwrap_or_default())
            }).map_err(err)?).map_err(err)?;
            let s = state.clone();
            g.set("Holds", lua.create_function(move |_, k: String| Ok(s.borrow().tags.iter().any(|(tk, _)| *tk == k))).map_err(err)?)
                .map_err(err)?;
            let s = state.clone();
            g.set("Accept", lua.create_function(move |_, ()| {
                s.borrow_mut().accepted = true;
                Ok(())
            }).map_err(err)?).map_err(err)?;
            let s = state.clone();
            g.set("Id", lua.create_function(move |_, ()| Ok(s.borrow().id)).map_err(err)?).map_err(err)?;
        }
        lua.load(&src).set_name(format!("@{}", path.display())).exec().map_err(err)?;
        let g = lua.globals();
        let keys = |name: &str| -> Result<Option<TagFilter>, String> {
            match g.get::<Value>(name).map_err(err)? {
                Value::Table(t) => {
                    let raw: Vec<String> = t.sequence_values::<String>().collect::<mlua::Result<_>>().map_err(err)?;
                    TagFilter::parse(&raw).map(Some).map_err(|e| format!("{name}: {e}"))
                }
                Value::Nil => Ok(None),
                _ => Err(format!("{name}: {name} isn't a table")),
            }
        };
        let way_keys = keys("way_keys")?;
        let node_keys = keys("node_keys")?;
        let node_function = match g.get::<Value>("node_function").map_err(err)? {
            Value::Function(f) => !is_empty_function(&lua, &f, &src).map_err(err)?,
            _ => false,
        };
        let scan = match g.get::<Value>("relation_scan_function").map_err(err)? {
            Value::Function(f) => Some(f),
            _ => None,
        };
        Ok(Profile { rules: Rules { name, way_keys, node_keys, node_function }, scan, state, _lua: lua })
    }

    pub fn has_scan(&self) -> bool {
        self.scan.is_some()
    }

    /// Whether relation_scan_function accepts a (non-multipolygon) relation.
    pub fn accepts(&self, id: i64, tags: &[(&[u8], &[u8])]) -> Result<bool, String> {
        let Some(scan) = &self.scan else { return Ok(false) };
        {
            let mut s = self.state.borrow_mut();
            s.id = id;
            s.accepted = false;
            s.tags.clear();
            s.tags.extend(tags.iter().map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), String::from_utf8_lossy(v).into_owned())));
        }
        scan.call::<()>(()).map_err(|e| format!("{}: relation_scan_function on relation {id}: {e}", self.rules.name))?;
        Ok(self.state.borrow().accepted)
    }
}

/// Whether a Lua function's body is empty (whitespace and comments only),
/// from its source lines.
fn is_empty_function(lua: &Lua, f: &Function, src: &str) -> mlua::Result<bool> {
    let debug: Table = lua.globals().get("debug")?;
    let getinfo: Function = debug.get("getinfo")?;
    let info: Table = getinfo.call((f.clone(), "S"))?;
    let (first, last): (usize, usize) = (info.get("linedefined")?, info.get("lastlinedefined")?);
    if first == 0 {
        return Ok(false);
    }
    let text: String = src
        .lines()
        .skip(first - 1)
        .take(last + 1 - first)
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    let Some(open) = text.find(')') else { return Ok(false) };
    let body = text[open + 1..].trim();
    Ok(body == "end")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags<'a>(t: &'a [(&'a str, &'a str)]) -> Vec<(&'a [u8], &'a [u8])> {
        t.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())).collect()
    }

    #[test]
    fn tag_filters_follow_tilemaker() {
        let f = TagFilter::parse(&["natural=water".into(), "waterway".into()]).unwrap();
        assert!(f.matches(tags(&[("natural", "water")])));
        assert!(f.matches(tags(&[("waterway", "stream"), ("name", "x")])));
        assert!(!f.matches(tags(&[("natural", "wood")])));
        assert!(!f.matches(tags(&[])));
        let none = TagFilter::parse(&[]).unwrap();
        assert!(!none.matches(tags(&[("highway", "path")])));
        let reject = TagFilter::parse(&["~building".into(), "~source".into()]).unwrap();
        assert!(!reject.matches(tags(&[("building", "yes"), ("source", "x")])));
        assert!(reject.matches(tags(&[("building", "yes"), ("name", "x")])));
        assert!(TagFilter::parse(&["a".into(), "~b".into()]).is_err());
    }

    fn rules(way: Option<&[&str]>, node: Option<&[&str]>, node_function: bool) -> Rules {
        let f = |k: Option<&[&str]>| k.map(|k| TagFilter::parse(&k.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap());
        Rules { name: "p".into(), way_keys: f(way), node_keys: f(node), node_function }
    }

    #[test]
    fn coverage_allows_narrowing_not_widening() {
        let old = [rules(Some(&["natural=water", "waterway"]), Some(&["waterway=dam"]), true)];
        // Same, narrower, or covered by a key-only filter: fine.
        assert!(rules(Some(&["natural=water", "waterway"]), Some(&["waterway=dam"]), true).uncovered_by(&old).is_none());
        assert!(rules(Some(&["waterway=river"]), None, false).uncovered_by(&old).is_none());
        // Wider ways or nodes, every way, every tagged node: not.
        assert!(rules(Some(&["natural"]), None, false).uncovered_by(&old).is_some());
        assert!(rules(None, None, false).uncovered_by(&old).is_some());
        assert!(rules(Some(&["waterway"]), Some(&["waterway"]), true).uncovered_by(&old).is_some());
        assert!(rules(Some(&["waterway"]), None, true).uncovered_by(&old).is_some());
        // A union of profiles covers each of them.
        let group = [rules(Some(&["highway"]), None, false), rules(Some(&["building"]), Some(&["amenity"]), true)];
        assert!(rules(Some(&["highway=path", "building"]), Some(&["amenity=bench"]), true).uncovered_by(&group).is_none());
        // Every way covers any way_keys.
        assert!(rules(Some(&["anything"]), None, false).uncovered_by(&[rules(None, None, false)]).is_none());
    }
}
