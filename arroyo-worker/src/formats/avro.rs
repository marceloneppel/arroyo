use std::collections::HashMap;
use std::sync::Arc;
use apache_avro::{from_avro_datum, Reader, Schema};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use arroyo_rpc::formats::AvroFormat;
use arroyo_rpc::schema_resolver::SchemaResolver;
use arroyo_types::UserError;

pub async fn deserialize_slice_avro<'a, T: DeserializeOwned>(
    format: &AvroFormat,
    schema_registry: Arc<Mutex<HashMap<u32, Schema>>>,
    resolver: Arc<dyn SchemaResolver + Sync>,
    mut msg: &'a [u8],
) -> Result<impl Iterator<Item = Result<T, UserError>> + 'a, String> {
    let id = if format.confluent_schema_registry {
        let magic_byte = msg[0];
        if magic_byte != 0 {
            return Err(format!("data was not encoded with schema registry wire format; magic byte has unexpected value: {}", magic_byte));
        }

        let id = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]);
        msg = &msg[5..];
        id
    } else {
        0
    };

    let mut registry = schema_registry.lock().await;

    let messages = if format.embedded_schema {
        Reader::new(&msg[..]).map_err(|e| format!("invalid Avro schema in message: {:?}", e))?
            .collect()
    } else {
        let schema = if registry.contains_key(&id) {
            registry.get(&id).unwrap()
        } else {
            let new_schema = resolver.resolve_schema(id).await?.ok_or_else(|| {
                format!(
                    "could not resolve schema for message with id {}",
                    id
                )
            })?;

            let new_schema = Schema::parse_str(&new_schema)
                .map_err(|e| format!("schema from Confluent Schema registry is not valid: {:?}", e))?;

            registry.insert(id, new_schema);

            registry.get(&id).unwrap()
        };

        if format.confluent_schema_registry {
            let mut buf = &msg[..];
            vec![from_avro_datum(schema, &mut buf, None)]
        } else {
            let reader = Reader::with_schema(schema, &msg[..])
                .map_err(|e| format!("invalid avro data: {:?}", e))?;

            reader.collect()
        }
    };

    Ok(messages.into_iter().map(|record| {
        apache_avro::from_value::<T>(&record.map_err(|e| {
            UserError::new(
                "Deserialization failed",
                format!(
                    "Failed to deserialize from avro: {:?}",
                    e
                ),
            )
        })?)
            .map_err(|e| {
                UserError::new(
                    "Deserialization failed",
                    format!("Failed to convert avro message into struct type: {:?}", e),
                )
            })
    }))


    // let record = reader.next()
    //     .ok_or_else(|| "avro record did not contain any messages")?
    //     .map_err(|e| e.to_string())?;
}


#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use async_trait::async_trait;
    use arroyo_rpc::formats::{AvroFormat, Format};
    use arroyo_rpc::schema_resolver::SchemaResolver;
    use crate::formats::DataDeserializer;
    use crate::SchemaData;

    struct TestSchemaResolver {
        schema: String,
        id: u32,
    }

    #[async_trait]
    impl SchemaResolver for TestSchemaResolver {
        async fn resolve_schema(&self, id: u32) -> Result<Option<String>, String> {
            assert_eq!(id, self.id);

            Ok(Some(self.schema.clone()))
        }
    }

    #[derive(
    Clone,
    Debug,
    bincode::Encode,
    bincode::Decode,
    PartialEq,
    PartialOrd,
    serde::Serialize,
    serde::Deserialize
    )]
    pub struct ArroyoAvroRoot {
        pub store_id: i32,
        pub store_order_id: i32,
        pub coupon_code: i32,
        pub date: i64,
        pub status: String,
        #[serde(deserialize_with = "crate::deserialize_raw_json")]
        pub order_lines: String,
    }

    #[derive(
    Clone,
    Debug,
    bincode::Encode,
    bincode::Decode,
    PartialEq,
    PartialOrd,
    serde::Serialize,
    serde::Deserialize
    )]
    pub struct OrderLine {
        pub product_id: i32,
        pub category: String,
        pub quantity: i32,
        pub unit_price: f64,
        pub net_price: f64,
    }

    impl SchemaData for ArroyoAvroRoot {
        fn name() -> &'static str {
            "ArroyoAvroRoot"
        }
        fn schema() -> arrow::datatypes::Schema {
            let fields: Vec<arrow::datatypes::Field> = vec![
                arrow::datatypes::Field::new("store_id",
                                             arrow::datatypes::DataType::Int32, false),
                arrow::datatypes::Field::new("store_order_id",
                                             arrow::datatypes::DataType::Int32, false),
                arrow::datatypes::Field::new("coupon_code",
                                             arrow::datatypes::DataType::Int32, false),
                arrow::datatypes::Field::new("date", arrow::datatypes::DataType::Utf8,
                                             false),
                arrow::datatypes::Field::new("status",
                                             arrow::datatypes::DataType::Utf8, false),
                arrow::datatypes::Field::new("order_lines",
                                             arrow::datatypes::DataType::Utf8, false),
            ];
            arrow::datatypes::Schema::new(fields)
        }
        fn to_raw_string(&self) -> Option<Vec<u8>> {
            unimplemented!("to_raw_string is not implemented for this type")
        }
    }

    #[tokio::test]
    async fn test_avro_deserialization() {
        let schema = r#"
        {
  "connect.name": "pizza_orders.pizza_orders",
  "fields": [
    {
      "name": "store_id",
      "type": "int"
    },
    {
      "name": "store_order_id",
      "type": "int"
    },
    {
      "name": "coupon_code",
      "type": "int"
    },
    {
      "name": "date",
      "type": {
        "connect.name": "org.apache.kafka.connect.data.Date",
        "connect.version": 1,
        "logicalType": "date",
        "type": "int"
      }
    },
    {
      "name": "status",
      "type": "string"
    },
    {
      "name": "order_lines",
      "type": {
        "items": {
          "connect.name": "pizza_orders.order_line",
          "fields": [
            {
              "name": "product_id",
              "type": "int"
            },
            {
              "name": "category",
              "type": "string"
            },
            {
              "name": "quantity",
              "type": "int"
            },
            {
              "name": "unit_price",
              "type": "double"
            },
            {
              "name": "net_price",
              "type": "double"
            }
          ],
          "name": "order_line",
          "type": "record"
        },
        "type": "array"
      }
    }
  ],
  "name": "pizza_orders",
  "namespace": "pizza_orders",
  "type": "record"
}"#;

        let message = [0u8, 0, 0, 0, 1, 8, 200, 223, 1, 144, 31, 186, 159, 2, 16, 97, 99, 99,
            101, 112, 116, 101, 100, 4, 156, 1, 10, 112, 105, 122, 122, 97, 4, 102, 102, 102, 102, 102,
            230, 38, 64, 102, 102, 102, 102, 102, 230, 54, 64, 84, 14, 100, 101, 115, 115, 101, 114, 116,
            2, 113, 61, 10, 215, 163, 112, 26, 64, 113, 61, 10, 215, 163, 112, 26, 64, 0, 10];

        let mut deserializer = DataDeserializer::with_schema_resolver(
            Format::Avro(AvroFormat {
                confluent_schema_registry: true,
                embedded_schema: false,
            }),
            None,
            Arc::new(TestSchemaResolver {
                schema: schema.to_string(),
                id: 1,
            }));

        let v: Vec<Result<ArroyoAvroRoot, _>> = deserializer.deserialize_slice(&message[..]).await.collect();

        for i in v {
            println!("{:?}", i.unwrap());
        }
    }
}