use serde::Deserialize;
#[derive(Deserialize, Clone, Debug)]
pub struct LoggingConfigurations0Config {
    #[serde(alias = "afterCallingApi")]
    pub after_calling_api: Option<bool>,
    #[serde(alias = "alsoLogToMessageLogs")]
    pub also_log_to_message_logs: Option<bool>,
    #[serde(alias = "beforeCallingApi")]
    pub before_calling_api: Option<bool>,
    #[serde(alias = "category")]
    pub category: Option<String>,
    #[serde(alias = "conditional", default, deserialize_with = "de_conditional_0")]
    pub conditional: Option<pdk::script::Script>,
    #[serde(alias = "configurationName")]
    pub configuration_name: String,
    #[serde(alias = "level")]
    pub level: Option<String>,
    #[serde(alias = "message", deserialize_with = "de_message_1")]
    pub message: pdk::script::Script,
}
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "batch_max_size")]
    pub batch_max_size: Option<i64>,
    #[serde(alias = "export_timeout_ms")]
    pub export_timeout_ms: Option<i64>,
    #[serde(alias = "loggingConfigurations")]
    pub logging_configurations: Vec<LoggingConfigurations0Config>,
    #[serde(alias = "otlp_api_key")]
    pub otlp_api_key: String,
    #[serde(
        alias = "otlp_endpoint",
        deserialize_with = "pdk::serde::deserialize_service"
    )]
    pub otlp_endpoint: pdk::hl::Service,
}
#[pdk::hl::entrypoint_flex]
fn init(abi: &dyn pdk::flex_abi::api::FlexAbi) -> Result<(), anyhow::Error> {
    let config: Config = serde_json::from_slice(abi.get_configuration())
        .map_err(|err| {
            anyhow::anyhow!(
                "Failed to parse configuration '{}'. Cause: {}",
                String::from_utf8_lossy(abi.get_configuration()), err
            )
        })?;
    abi.service_create(config.otlp_endpoint)?;
    abi.setup()?;
    Ok(())
}
fn de_conditional_0<'de, D>(
    deserializer: D,
) -> Result<Option<pdk::script::Script>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let exp: Option<pdk::script::Expression> = serde::de::Deserialize::deserialize(
        deserializer,
    )?;
    exp.map(|exp| {
            pdk::script::ScriptingEngine::script(&exp)
                .input(pdk::script::Input::Attributes)
                .input(pdk::script::Input::Authentication)
                .input(pdk::script::Input::Payload(pdk::script::Format::Json))
                .input(pdk::script::Input::Payload(pdk::script::Format::Xml))
                .input(pdk::script::Input::Payload(pdk::script::Format::PlainText))
                .compile()
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}
fn de_message_1<'de, D>(deserializer: D) -> Result<pdk::script::Script, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let exp: pdk::script::Expression = serde::de::Deserialize::deserialize(
        deserializer,
    )?;
    pdk::script::ScriptingEngine::script(&exp)
        .input(pdk::script::Input::Attributes)
        .input(pdk::script::Input::Authentication)
        .input(pdk::script::Input::Payload(pdk::script::Format::Json))
        .input(pdk::script::Input::Payload(pdk::script::Format::Xml))
        .input(pdk::script::Input::Payload(pdk::script::Format::PlainText))
        .compile()
        .map_err(serde::de::Error::custom)
}
