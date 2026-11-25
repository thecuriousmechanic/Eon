use std::io;

use serde_json::Value;

use super::invalid;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sync {
    #[default]
    None,
    OpenClose,
    Full,
    Incremental,
}

impl Sync {
    fn parse(value: Option<&Value>) -> io::Result<Self> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(Self::None);
        };
        // Legacy numeric Full/Incremental imply didOpen + didClose.
        let (open, kind) = if value.is_object() {
            (value["openClose"] == true, value.get("change"))
        } else {
            (value != 0, Some(value))
        };
        let mode = match kind {
            None => Self::OpenClose,
            Some(v) => match v.as_u64() {
                Some(0) => Self::OpenClose,
                Some(1) => Self::Full,
                Some(2) => Self::Incremental,
                _ => return Err(invalid("invalid text document sync capability")),
            },
        };
        Ok(if open { mode } else { Self::None })
    }
}

#[derive(Default)]
pub(super) enum Diagnostics {
    #[default]
    Push,
    Pull { identifier: Option<String> },
}

#[derive(Default)]
pub(super) struct Capabilities {
    pub sync: Sync,
    pub hover: bool,
    pub definition: bool,
    pub references: bool,
    pub symbols: bool,
    pub diagnostics: Diagnostics,
}

impl Capabilities {
    pub fn parse(caps: &Value) -> io::Result<Self> {
        let caps = caps.as_object().ok_or_else(|| invalid("missing server capabilities"))?;
        if caps.get("positionEncoding").is_some_and(|s| s != "utf-16") {
            return Err(invalid("unsupported position encoding"));
        }
        let provider = |name| caps.get(name).is_some_and(|v| v == true || v.is_object());
        let diagnostics = match caps.get("diagnosticProvider").and_then(Value::as_object) {
            None => Diagnostics::Push,
            Some(options) => Diagnostics::Pull {
                identifier: options
                    .get("identifier")
                    .map(|v| {
                        v.as_str()
                            .filter(|s| s.len() <= 256)
                            .map(str::to_owned)
                            .ok_or_else(|| invalid("invalid diagnostic provider identifier"))
                    })
                    .transpose()?,
            },
        };
        Ok(Self {
            sync: Sync::parse(caps.get("textDocumentSync"))?,
            hover: provider("hoverProvider"),
            definition: provider("definitionProvider"),
            references: provider("referencesProvider"),
            symbols: provider("workspaceSymbolProvider"),
            diagnostics,
        })
    }
}

pub(super) fn require(supported: bool, feature: &'static str) -> io::Result<()> {
    if supported {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Unsupported, feature))
    }
}
