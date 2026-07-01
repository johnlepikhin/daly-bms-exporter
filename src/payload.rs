//! Wire types for the telemetry POST body (`SaveThingInfo1`). See
//! `doc/daly-bms-protocol.md` §2.1.

use serde::Deserialize;

/// Body of `POST /api/v2/http2/SaveThingInfo1`.
#[derive(Debug, Deserialize)]
pub struct TelemetryBody {
    #[serde(rename = "DeviceName", default)]
    pub device_name: String,
    #[serde(rename = "Sn")]
    pub sn: String,
    #[serde(rename = "Data", default)]
    pub data: Vec<DataEntry>,
}

/// A single request→response Modbus pair inside the telemetry body.
#[derive(Debug, Deserialize)]
pub struct DataEntry {
    /// Hex of the Modbus request (used to dispatch the register block).
    #[serde(rename = "Command")]
    pub command: String,
    /// Hex of the Modbus response (the actual register data).
    #[serde(rename = "Data")]
    pub data: String,
    #[serde(rename = "TimeStamp", default)]
    pub timestamp: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_documented_body() {
        let json = r#"{"DeviceName":"dev","Sn":"224KE220900366","Data":[
            {"Command":"D2030000007ED649","TimeStamp":"1782884514214","Data":"D203FC00"}
        ]}"#;
        let body: TelemetryBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.sn, "224KE220900366");
        assert_eq!(body.data.len(), 1);
        assert_eq!(body.data[0].command, "D2030000007ED649");
    }
}
