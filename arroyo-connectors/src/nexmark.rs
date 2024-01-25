use anyhow::{anyhow, bail};
use arroyo_operator::connector::{Connection, Connector};
use arroyo_rpc::api_types::connections::FieldType::Primitive;
use arroyo_rpc::api_types::connections::{
    ConnectionProfile, ConnectionSchema, ConnectionType, FieldType, SourceFieldType, StructType,
    TestSourceMessage,
};
use arroyo_rpc::OperatorConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use typify::import_types;

use crate::{nullable_field, pull_opt, source_field, EmptyConfig};

const TABLE_SCHEMA: &str = include_str!("../../connector-schemas/nexmark/table.json");
const ICON: &str = include_str!("../resources/nexmark.svg");

import_types!(schema = "../connector-schemas/nexmark/table.json");

pub fn nexmark_schema() -> ConnectionSchema {
    use arroyo_rpc::api_types::connections::PrimitiveType::*;
    ConnectionSchema {
        format: None,
        bad_data: None,
        framing: None,
        struct_name: Some("arroyo_types::nexmark::Event".to_string()),
        fields: vec![
            nullable_field(
                "person",
                SourceFieldType {
                    r#type: FieldType::Struct(StructType {
                        name: Some("arroyo_types::nexmark::Person".to_string()),
                        fields: vec![
                            source_field("id", Primitive(Int64)),
                            source_field("name", Primitive(String)),
                            source_field("email_address", Primitive(String)),
                            source_field("credit_card", Primitive(String)),
                            source_field("city", Primitive(String)),
                            source_field("state", Primitive(String)),
                            source_field("datetime", Primitive(UnixMillis)),
                            source_field("extra", Primitive(String)),
                        ],
                    }),
                    sql_name: None,
                },
            ),
            nullable_field(
                "bid",
                SourceFieldType {
                    r#type: FieldType::Struct(StructType {
                        name: Some("arroyo_types::nexmark::Bid".to_string()),
                        fields: vec![
                            source_field("auction", Primitive(Int64)),
                            source_field("bidder", Primitive(Int64)),
                            source_field("price", Primitive(Int64)),
                            source_field("channel", Primitive(String)),
                            source_field("url", Primitive(String)),
                            source_field("datetime", Primitive(UnixMillis)),
                            source_field("extra", Primitive(String)),
                        ],
                    }),
                    sql_name: None,
                },
            ),
            nullable_field(
                "auction",
                SourceFieldType {
                    r#type: FieldType::Struct(StructType {
                        name: Some("arroyo_types::nexmark::Auction".to_string()),
                        fields: vec![
                            source_field("id", Primitive(Int64)),
                            source_field("description", Primitive(String)),
                            source_field("item_name", Primitive(String)),
                            source_field("initial_bid", Primitive(Int64)),
                            source_field("reserve", Primitive(Int64)),
                            source_field("datetime", Primitive(UnixMillis)),
                            source_field("expires", Primitive(UnixMillis)),
                            source_field("seller", Primitive(Int64)),
                            source_field("category", Primitive(Int64)),
                            source_field("extra", Primitive(String)),
                        ],
                    }),
                    sql_name: None,
                },
            ),
        ],
        definition: None,
        inferred: None,
    }
}

pub struct NexmarkConnector {}

impl Connector for NexmarkConnector {
    type ProfileT = EmptyConfig;
    type TableT = NexmarkTable;

    fn name(&self) -> &'static str {
        "nexmark"
    }

    fn metadata(&self) -> arroyo_rpc::api_types::connections::Connector {
        arroyo_rpc::api_types::connections::Connector {
            id: "nexmark".to_string(),
            name: "Nexmark".to_string(),
            icon: ICON.to_string(),
            description: "Demo source for a simulated auction website".to_string(),
            enabled: true,
            source: true,
            sink: false,
            testing: false,
            hidden: false,
            custom_schemas: false,
            connection_config: None,
            table_config: TABLE_SCHEMA.to_string(),
        }
    }

    fn table_type(&self, _: Self::ProfileT, _: Self::TableT) -> ConnectionType {
        return ConnectionType::Source;
    }

    fn get_schema(
        &self,
        _: Self::ProfileT,
        _: Self::TableT,
        _: Option<&ConnectionSchema>,
    ) -> Option<ConnectionSchema> {
        Some(nexmark_schema())
    }

    fn test(
        &self,
        _: &str,
        _: Self::ProfileT,
        _: Self::TableT,
        _: Option<&ConnectionSchema>,
        tx: tokio::sync::mpsc::Sender<TestSourceMessage>,
    ) {
        tokio::task::spawn(async move {
            let message = TestSourceMessage {
                error: false,
                done: true,
                message: "Successfully validated connection".to_string(),
            };
            tx.send(message).await.unwrap();
        });
    }

    fn from_options(
        &self,
        name: &str,
        options: &mut HashMap<String, String>,
        schema: Option<&ConnectionSchema>,
        _profile: Option<&ConnectionProfile>,
    ) -> anyhow::Result<Connection> {
        let event_rate = f64::from_str(&pull_opt("event_rate", options)?)
            .map_err(|_| anyhow!("invalid value for event_rate; expected float"))?;

        let runtime = options
            .remove("runtime")
            .map(|t| f64::from_str(&t))
            .transpose()
            .map_err(|_| anyhow!("invalid value for runtime; expected float"))?;

        if let Some(schema) = schema {
            if !schema.fields.is_empty() && schema.fields != nexmark_schema().fields {
                bail!("invalid schema for nexmark source; omit fields to rely on inference");
            }
        }

        self.from_config(
            None,
            name,
            EmptyConfig {},
            NexmarkTable {
                event_rate,
                runtime,
            },
            None,
        )
    }

    fn from_config(
        &self,
        id: Option<i64>,
        name: &str,
        config: Self::ProfileT,
        table: Self::TableT,
        _: Option<&ConnectionSchema>,
    ) -> anyhow::Result<Connection> {
        let description = format!(
            "{}Nexmark<{} eps>",
            if table.runtime.is_some() {
                "Bounded"
            } else {
                ""
            },
            table.event_rate
        );

        let config = OperatorConfig {
            connection: serde_json::to_value(config).unwrap(),
            table: serde_json::to_value(table).unwrap(),
            rate_limit: None,
            format: None,
            bad_data: None,
            framing: None,
        };

        Ok(Connection {
            id,
            connector: self.name(),
            name: name.to_string(),
            connection_type: ConnectionType::Source,
            schema: nexmark_schema(),
            config: serde_json::to_string(&config).unwrap(),
            description,
        })
    }
}
