use serde::Deserialize;
#[derive(Deserialize, Clone, Debug)]
pub struct ToolOverridesItem {
    #[serde(alias = "maximumRequests")]
    pub maximum_requests: i64,
    #[serde(alias = "timePeriodInMilliseconds")]
    pub time_period_in_milliseconds: i64,
    #[serde(alias = "toolName")]
    pub tool_name: String,
}
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "keySelector", deserialize_with = "de_key_selector_0")]
    pub key_selector: pdk::script::Script,
    #[serde(alias = "maximumRequests")]
    pub maximum_requests: i64,
    #[serde(alias = "timePeriodInMilliseconds")]
    pub time_period_in_milliseconds: i64,
    #[serde(alias = "toolOverrides")]
    pub tool_overrides: Option<Vec<ToolOverridesItem>>,
    #[serde(alias = "unmeteredTools")]
    pub unmetered_tools: Option<Vec<String>>,
}
#[pdk::hl::entrypoint_flex]
fn init(abi: &dyn pdk::flex_abi::api::FlexAbi) -> Result<(), anyhow::Error> {
    abi.setup()?;
    Ok(())
}
fn de_key_selector_0<'de, D>(deserializer: D) -> Result<pdk::script::Script, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let exp: pdk::script::Expression = serde::de::Deserialize::deserialize(
        deserializer,
    )?;
    pdk::script::ScriptingEngine::script(&exp)
        .input(pdk::script::Input::Attributes)
        .input(pdk::script::Input::Vars("toolName"))
        .compile()
        .map_err(serde::de::Error::custom)
}
