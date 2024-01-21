use arrow::datatypes::{DataType, TimeUnit};
use arrow_schema::{Field, Schema, SchemaRef};
use datafusion_common::{DFField, DFSchema, DFSchemaRef, Result as DFResult};
use std::{collections::HashMap, sync::Arc};

pub fn window_arrow_struct() -> DataType {
    DataType::Struct(
        vec![
            Arc::new(arrow::datatypes::Field::new(
                "start",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            )),
            Arc::new(arrow::datatypes::Field::new(
                "end",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            )),
        ]
        .into(),
    )
}

pub(crate) fn add_timestamp_field(schema: DFSchemaRef) -> DFResult<DFSchemaRef> {
    let timestamp_field = DFField::new_unqualified(
        "_timestamp",
        DataType::Timestamp(TimeUnit::Nanosecond, None),
        false,
    );
    Ok(Arc::new(schema.join(&DFSchema::new_with_metadata(
        vec![timestamp_field],
        HashMap::new(),
    )?)?))
}

pub(crate) fn has_timestamp_field(schema: DFSchemaRef) -> bool {
    schema
        .fields()
        .iter()
        .any(|field| field.name() == "_timestamp")
}

pub(crate) fn has_timestamp_field_arrow(schema: SchemaRef) -> bool {
    schema.fields.iter().any(|f| f.name() == "_timestamp")
}

pub fn add_timestamp_field_arrow(schema: SchemaRef) -> SchemaRef {
    let mut fields = schema.fields().to_vec();
    fields.push(Arc::new(Field::new(
        "_timestamp",
        DataType::Timestamp(TimeUnit::Nanosecond, None),
        false,
    )));
    Arc::new(Schema::new(fields))
}

pub(crate) fn add_timestamp_field_if_missing_arrow(schema: SchemaRef) -> SchemaRef {
    if has_timestamp_field_arrow(schema.clone()) {
        schema
    } else {
        add_timestamp_field_arrow(schema)
    }
}
