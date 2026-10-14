use crate::{
    chap,
    pdu::{invalid, MAX_SEGMENT},
    TargetConfig,
};
use rand::Rng;
use std::io;

#[derive(Clone)]
pub(crate) struct Negotiation {
    pub peer_segment: usize,
    pub initial_r2t: bool,
    pub immediate_data: bool,
    pub first_burst: usize,
    pub max_burst: usize,
    pub discovery: bool,
    pub authenticated: bool,
    pub challenge: Option<(u8, [u8; 32])>,
    pub target_seen: bool,
}

impl Default for Negotiation {
    fn default() -> Self {
        Self {
            peer_segment: 8192,
            initial_r2t: true,
            immediate_data: true,
            first_burst: 65536,
            max_burst: MAX_SEGMENT,
            discovery: false,
            authenticated: false,
            challenge: None,
            target_seen: false,
        }
    }
}

impl Negotiation {
    pub fn security(&mut self, cfg: &TargetConfig, keys: &[(String, String)]) -> Result<Vec<(String, String)>, u16> {
        let find = |name: &str| keys.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        if let Some(kind) = find("SessionType") {
            if kind != "Discovery" && kind != "Normal" {
                return Err(0x0200);
            }
            self.discovery = kind == "Discovery";
        }
        if let Some(name) = find("TargetName") {
            if name != cfg.iqn {
                return Err(0x0203);
            }
            self.target_seen = true;
        }
        let method = if cfg.chap.is_some() { "CHAP" } else { "None" };
        let mut reply = Vec::new();
        if find("TargetName").is_some() && !self.discovery {
            reply.extend([
                ("TargetPortalGroupTag".into(), "1".into()),
                ("TargetAlias".into(), "inocrium reflect".into()),
            ]);
        }
        if let Some(offered) = find("AuthMethod") {
            if !offered.split(',').any(|v| v == method) {
                return Err(0x0201);
            }
            reply.push(("AuthMethod".into(), method.into()));
        }
        if let Some(secret) = &cfg.chap {
            if let Some(algorithm) = find("CHAP_A") {
                if !algorithm.split(',').any(|a| a == "5") {
                    return Err(0x0201);
                }
                let mut rng = rand::thread_rng();
                let challenge: (u8, [u8; 32]) = (rng.gen(), rng.gen());
                reply.extend([
                    ("CHAP_A".into(), "5".into()),
                    ("CHAP_I".into(), challenge.0.to_string()),
                    ("CHAP_C".into(), chap::hex(&challenge.1)),
                ]);
                self.challenge = Some(challenge);
            }
            if let Some(received) = find("CHAP_R") {
                let (id, challenge) = self.challenge.take().ok_or(0x0201u16)?;
                let bytes = chap::unhex(received).ok_or(0x0201u16)?;
                if find("CHAP_N") != Some(secret.user.as_str())
                    || !chap::verify(&bytes, &chap::response(id, &secret.secret, &challenge))
                {
                    return Err(0x0201);
                }
                // Mutual CHAP is deliberately unsupported: never reflect credentials.
                if find("CHAP_I").is_some() || find("CHAP_C").is_some() {
                    return Err(0x0201);
                }
                self.authenticated = true;
            }
        } else {
            self.authenticated = true;
        }
        Ok(reply)
    }

    pub fn operational(&mut self, keys: &[(String, String)]) -> io::Result<Vec<(String, String)>> {
        let mut reply = Vec::new();
        if let Some((_, value)) = keys.iter().find(|(k, _)| k == "MaxBurstLength") {
            self.max_burst = number(value, 512, 0xff_ffff)?.min(MAX_SEGMENT);
        }
        for (key, value) in keys {
            let selected = match key.as_str() {
                "HeaderDigest" | "DataDigest" => {
                    if !value.split(',').any(|v| v == "None") {
                        return Err(invalid("digest None not offered"));
                    }
                    "None".into()
                }
                "MaxConnections" | "MaxOutstandingR2T" => {
                    number(value, 1, 65535)?;
                    "1".into()
                }
                "InitialR2T" => {
                    self.initial_r2t = boolean(value)?;
                    value.clone()
                }
                "ImmediateData" => {
                    self.immediate_data = boolean(value)?;
                    value.clone()
                }
                "MaxRecvDataSegmentLength" => {
                    self.peer_segment = number(value, 512, 0xff_ffff)?;
                    MAX_SEGMENT.to_string()
                }
                "MaxBurstLength" => {
                    self.max_burst = number(value, 512, 0xff_ffff)?.min(MAX_SEGMENT);
                    self.max_burst.to_string()
                }
                "FirstBurstLength" => {
                    self.first_burst = number(value, 512, 0xff_ffff)?.min(65536).min(self.max_burst);
                    self.first_burst.to_string()
                }
                "DefaultTime2Wait" => {
                    let n = number(value, 0, 3600)?;
                    n.max(2).to_string()
                }
                "DefaultTime2Retain" => {
                    number(value, 0, 3600)?;
                    "0".into()
                }
                "DataPDUInOrder" | "DataSequenceInOrder" => {
                    boolean(value)?;
                    "Yes".into()
                }
                "ErrorRecoveryLevel" => {
                    number(value, 0, 2)?;
                    "0".into()
                }
                "IFMarker" | "OFMarker" => {
                    boolean(value)?;
                    "No".into()
                }
                "IFMarkInt" | "OFMarkInt" => "Irrelevant".into(),
                "TargetPortalGroupTag" | "TargetAlias" => continue,
                "InitiatorName" | "InitiatorAlias" | "TargetName" | "SessionType" | "AuthMethod" | "CHAP_A"
                | "CHAP_I" | "CHAP_C" | "CHAP_N" | "CHAP_R" => continue,
                _ => "NotUnderstood".into(),
            };
            reply.push((key.clone(), selected));
        }
        self.first_burst = self.first_burst.min(self.max_burst);
        for (key, value) in &mut reply {
            if key == "FirstBurstLength" {
                *value = self.first_burst.to_string();
            }
        }
        reply.extend([("TargetPortalGroupTag".into(), "1".into()), ("TargetAlias".into(), "inocrium reflect".into())]);
        Ok(reply)
    }
}

fn number(value: &str, min: usize, max: usize) -> io::Result<usize> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| invalid("invalid numeric negotiation key"))
}
fn boolean(value: &str) -> io::Result<bool> {
    match value {
        "Yes" => Ok(true),
        "No" => Ok(false),
        _ => Err(invalid("invalid boolean negotiation key")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_parameters() {
        let mut n = Negotiation::default();
        n.operational(&[
            ("MaxRecvDataSegmentLength".into(), "4096".into()),
            ("MaxBurstLength".into(), "8192".into()),
            ("FirstBurstLength".into(), "4096".into()),
            ("InitialR2T".into(), "No".into()),
        ])
        .unwrap();
        assert_eq!(n.peer_segment, 4096);
        assert_eq!(n.max_burst, 8192);
        assert!(!n.initial_r2t);
        assert!(n.operational(&[("MaxRecvDataSegmentLength".into(), "0".into())]).is_err());
        assert!(n.operational(&[("DataDigest".into(), "CRC32C".into())]).is_err());
        let reply = n
            .operational(&[("FirstBurstLength".into(), "65536".into()), ("MaxBurstLength".into(), "512".into())])
            .unwrap();
        assert!(reply.contains(&("FirstBurstLength".into(), "512".into())));
    }
}
