use anyhow::{bail, ensure, Context, Result};

use inflector::Inflector;
use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec};

use crate::{
    go::{GoField, GoStruct, GoType},
    utils::suffixes,
};

pub trait SchemaExt {
    fn object(&self) -> anyhow::Result<&SchemaObject>;
}

impl SchemaExt for Schema {
    fn object(&self) -> anyhow::Result<&SchemaObject> {
        match self {
            Schema::Object(o) => Ok(o),
            _ => bail!("expected schema object"),
        }
    }
}

/// Returns the schemas of the variants of this enum, if it is an enum.
/// Returns `None` if the schema is not an enum.
pub fn enum_variants(schema: &SchemaObject) -> Option<Vec<&Schema>> {
    Some(
        schema
            .subschemas
            .as_ref()?
            .one_of
            .as_ref()?
            .iter()
            .collect(),
    )
}

/// Tries to extract the name and type of the given enum variant.
pub fn enum_variant(
    schema: &SchemaObject,
    enum_name: &str,
    additional_structs: &mut Vec<GoStruct>,
) -> Result<GoField> {
    // for variants without inner data, there is an entry in `enum_variants`
    // we are not interested in that case, so we error out
    if let Some(values) = &schema.enum_values {
        bail!(
            "enum variants {} without inner data not supported",
            values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let docs = documentation(schema);

    // for variants with inner data, there is an object validation entry with a single property
    // we extract the type of that property
    let properties = &schema
        .object
        .as_ref()
        .context("expected object validation for enum variant")?
        .properties;
    ensure!(
        properties.len() == 1,
        "expected exactly one property in enum variant"
    );
    // we can unwrap here, because we checked the length above
    let (name, schema) = properties.first_key_value().unwrap();
    let (ty, _) = schema_object_type(
        schema.object()?,
        TypeContext::new(enum_name, name),
        additional_structs,
    )?;

    Ok(GoField {
        rust_name: name.to_string(),
        docs,
        ty: GoType {
            name: ty,
            is_nullable: true, // always nullable
        },
    })
}

/// Returns the Go type for the given schema object and whether it is nullable.
/// May also add additional structs to the given `Vec` that need to be generated for this type.
pub fn schema_object_type(
    schema: &SchemaObject,
    type_context: TypeContext,
    additional_structs: &mut Vec<GoStruct>,
) -> Result<(String, bool)> {
    let mut is_nullable = is_null(schema);

    // if it has a title, use that
    let ty = if let Some(title) = schema.metadata.as_ref().and_then(|m| m.title.as_ref()) {
        replace_custom_type(title)
    } else if let Some(reference) = &schema.reference {
        // if it has a reference, strip the path and use that
        replace_custom_type(
            reference
                .split('/')
                .last()
                .expect("split should always return at least one item"),
        )
    } else if let Some(t) = &schema.instance_type {
        type_from_instance_type(schema, type_context, t, additional_structs)?
    } else if let Some(subschemas) = schema.subschemas.as_ref().and_then(|s| s.any_of.as_ref()) {
        // check if one of them is null
        let nullable = nullable_type(subschemas)?;
        if let Some(non_null) = nullable {
            ensure!(subschemas.len() == 2, "multiple subschemas in anyOf");
            is_nullable = true;
            // extract non-null type
            let (non_null_type, _) =
                schema_object_type(non_null, type_context, additional_structs)?;
            replace_custom_type(&non_null_type)
        } else {
            subschema_type(subschemas, type_context, additional_structs)
                .context("failed to get type of anyOf subschemas")?
        }
    } else if let Some(subschemas) = schema
        .subschemas
        .as_ref()
        .and_then(|s| s.all_of.as_ref().or(s.one_of.as_ref()))
    {
        subschema_type(subschemas, type_context, additional_structs)
            .context("failed to get type of allOf subschemas")?
    } else {
        bail!("no type for schema found: {:?}", schema);
    };

    Ok((ty, is_nullable))
}

/// Tries to extract the type of the non-null variant of an anyOf schema.
///
/// Returns `Ok(None)` if the type is not nullable.
pub fn nullable_type(subschemas: &[Schema]) -> Result<Option<&SchemaObject>, anyhow::Error> {
    let (found_null, nullable_type): (bool, Option<&SchemaObject>) = subschemas
        .iter()
        .fold(Ok((false, None)), |result: Result<_>, subschema| {
            result.and_then(|(nullable, not_null)| {
                let subschema = subschema.object()?;
                if is_null(subschema) {
                    Ok((true, not_null))
                } else {
                    Ok((nullable, Some(subschema)))
                }
            })
        })
        .context("failed to get anyOf subschemas")?;

    Ok(if found_null { nullable_type } else { None })
}

/// The context for type extraction
#[derive(Clone, Copy, Debug)]
pub struct TypeContext<'a> {
    /// The struct name
    struct_name: &'a str,
    /// The name of the field in the parent struct
    field: &'a str,
}

impl<'a> TypeContext<'a> {
    pub fn new(parent: &'a str, field: &'a str) -> Self {
        Self {
            struct_name: parent,
            field,
        }
    }
}

/// Tries to extract a type name from the given instance type.
///
/// Fails for unsupported instance types or integer formats.
pub fn type_from_instance_type(
    schema: &SchemaObject,
    type_context: TypeContext,
    t: &SingleOrVec<InstanceType>,
    additional_structs: &mut Vec<GoStruct>,
) -> Result<String> {
    // if it has an instance type, use that
    Ok(if t.contains(&InstanceType::String) {
        "string".to_string()
    } else if t.contains(&InstanceType::Number) {
        "float64".to_string()
    } else if t.contains(&InstanceType::Integer) {
        const AVAILABLE_INTS: &[&str] = &[
            "uint8", "int8", "uint16", "int16", "uint32", "int32", "uint64", "int64",
        ];
        let format = schema.format.as_deref().unwrap_or("int64");
        if AVAILABLE_INTS.contains(&format) {
            format.to_string()
        } else {
            bail!("unsupported integer format: {}", format);
        }
    } else if t.contains(&InstanceType::Boolean) {
        "bool".to_string()
    } else if t.contains(&InstanceType::Object) {
        // generate a new struct for this object
        // struct_name should be in PascalCase, so we detect the last word and use that as
        // the suffix for the new struct name
        let suffix = suffixes(type_context.struct_name)
            .rev()
            .find(|s| s.starts_with(char::is_uppercase))
            .unwrap_or(type_context.struct_name);
        let new_struct_name = format!("{}{suffix}", type_context.field.to_pascal_case());

        let fields = schema
            .object
            .as_ref()
            .context("expected object validation")?
            .properties
            .iter()
            .map(|(name, schema)| {
                let schema = schema.object()?;
                let (ty, is_nullable) = schema_object_type(
                    schema,
                    TypeContext::new(&new_struct_name, name),
                    additional_structs,
                )?;
                Ok(GoField {
                    rust_name: name.to_string(),
                    docs: documentation(schema),
                    ty: GoType {
                        name: ty,
                        is_nullable,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let strct = GoStruct {
            name: new_struct_name.clone(),
            docs: None,
            fields,
        };
        additional_structs.push(strct);

        new_struct_name
    } else if t.contains(&InstanceType::Array) {
        // get type of items
        let (item_type, item_nullable) = array_item_type(schema, type_context, additional_structs)
            .context("failed to get array item type")?;
        // map custom types
        let item_type = custom_type_of(&item_type).unwrap_or(&item_type);

        if item_nullable {
            replace_custom_type(&format!("[]*{}", item_type))
        } else {
            replace_custom_type(&format!("[]{}", item_type))
        }
    } else {
        unreachable!("instance type should be one of the above")
    })
}

/// Extract the type of the items of an array.
///
/// This fails if the given schema object is not an array,
/// has multiple item types or other errors occur during type extraction of
/// the underlying schema.
pub fn array_item_type(
    schema: &SchemaObject,
    type_context: TypeContext,
    additional_structs: &mut Vec<GoStruct>,
) -> Result<(String, bool)> {
    match schema.array.as_ref().and_then(|a| a.items.as_ref()) {
        Some(SingleOrVec::Single(array_validation)) => {
            schema_object_type(array_validation.object()?, type_context, additional_structs)
        }
        _ => bail!("array type with non-singular item type is not supported"),
    }
}

/// Tries to extract a type name from the given subschemas.
///
/// This fails if there are multiple subschemas or other errors occur
/// during subschema type extraction.
pub fn subschema_type(
    subschemas: &[Schema],
    type_context: TypeContext,
    additional_structs: &mut Vec<GoStruct>,
) -> Result<String> {
    ensure!(
        subschemas.len() == 1,
        "multiple subschemas are not supported"
    );
    let subschema = &subschemas[0];
    let (ty, _) = schema_object_type(subschema.object()?, type_context, additional_structs)?;
    Ok(replace_custom_type(&ty))
}

pub fn is_null(schema: &SchemaObject) -> bool {
    schema
        .instance_type
        .as_ref()
        .map_or(false, |s| s.contains(&InstanceType::Null))
}

pub fn documentation(schema: &SchemaObject) -> Option<String> {
    schema.metadata.as_ref()?.description.as_ref().cloned()
}

/// Maps special types to their Go equivalents.
/// If the given type is not a special type, returns `None`.
pub fn custom_type_of(ty: &str) -> Option<&str> {
    match ty {
        "Uint128" => Some("string"),
        "Binary" => Some("[]byte"),
        "HexBinary" => Some("Checksum"),
        "Addr" => Some("string"),
        "Decimal" => Some("string"),
        _ => None,
    }
}

pub fn replace_custom_type(ty: &str) -> String {
    custom_type_of(ty)
        .map(|ty| ty.to_string())
        .unwrap_or_else(|| ty.to_string())
}
