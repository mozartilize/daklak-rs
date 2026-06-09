//! IBusText and IBusAttrList GVariant builders.
//!
//! Wire formats (verified against vendors/ibus/src/ibustext.c,
//! ibusserializable.c, ibusattrlist.c):
//!
//!   IBusText     = (s a{sv} s v)   → ("IBusText",     {}, text, <attrlist>)
//!   IBusAttrList = (s a{sv} av)    → ("IBusAttrList", {}, [])
//!
//! Important: use StructureBuilder::append_field (not add_field / tuple From).
//! `add_field` runs Value::new which re-wraps any Value-typed argument as a
//! variant `v` (because Value's dynamic signature is "v"), corrupting
//! the `a{sv}` and `av` container fields.

use zvariant::{Array, Dict, Signature, StructureBuilder, Value};

fn empty_attachments() -> Dict<'static, 'static> {
    Dict::new(&Signature::Str, &Signature::Variant)
}

/// `IBusAttrList` with no attributes → `(sa{sv}av)`.
pub fn ibus_attr_list() -> Value<'static> {
    let empty_av = Array::new(&Signature::Variant);
    Value::Structure(
        StructureBuilder::new()
            .append_field(Value::from("IBusAttrList"))
            .append_field(Value::Dict(empty_attachments()))
            .append_field(Value::Array(empty_av))
            .build()
            .expect("non-empty structure"),
    )
}

/// `IBusText` wrapping `text` → `(sa{sv}sv)`.
pub fn ibus_text(text: &str) -> Value<'static> {
    let attrs = Value::Value(Box::new(ibus_attr_list()));
    Value::Structure(
        StructureBuilder::new()
            .append_field(Value::from("IBusText"))
            .append_field(Value::Dict(empty_attachments()))
            .append_field(Value::from(text.to_string()))
            .append_field(attrs)
            .build()
            .expect("non-empty structure"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ibus_text_signature() {
        assert_eq!(ibus_text("x").value_signature().to_string(), "(sa{sv}sv)");
    }
    #[test]
    fn ibus_attr_list_signature() {
        assert_eq!(ibus_attr_list().value_signature().to_string(), "(sa{sv}av)");
    }
}
