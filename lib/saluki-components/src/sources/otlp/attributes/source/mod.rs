//! OTLP source representation.

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SourceKind {
    HostnameKind,
    AwsEcsFargateKind,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub kind: SourceKind,
    pub identifier: String,
}

#[allow(dead_code)]
impl Source {
    pub fn tag(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.identifier)
    }
}

#[allow(dead_code)]
impl SourceKind {
    fn as_str(&self) -> &'static str {
        match self {
            SourceKind::HostnameKind => "host",
            SourceKind::AwsEcsFargateKind => "task_arn",
        }
    }
}
