//! How a connection is told apart from the others: the colour its explorer
//! entry and tab group share, and the environment it belongs to.
//!
//! Kept in `config.db`'s `settings` as `connection.<id>.appearance`
//! (`{"color": "red", "environment": "production"}`), like the row limit.
//! A missing or unreadable value is the default: the driver's colour and no
//! environment.
use jykre_core::ConnectionId;
use serde_json::Value;

/// The colours a connection can take. Their shades live with the other
/// colour tokens in the views.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionColor {
    Mint,
    Teal,
    Blue,
    Purple,
    Pink,
    Red,
    Amber,
    Slate,
}

impl ConnectionColor {
    pub const ALL: [Self; 8] = [
        Self::Mint,
        Self::Teal,
        Self::Blue,
        Self::Purple,
        Self::Pink,
        Self::Red,
        Self::Amber,
        Self::Slate,
    ];

    /// The name stored in the setting.
    pub fn key(self) -> &'static str {
        match self {
            Self::Mint => "mint",
            Self::Teal => "teal",
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Pink => "pink",
            Self::Red => "red",
            Self::Amber => "amber",
            Self::Slate => "slate",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|color| color.key() == key)
    }
}

/// What a connection is used for. Production asks before statements that
/// change every row or drop data ([`crate::guard`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    pub const ALL: [Self; 3] = [Self::Development, Self::Staging, Self::Production];

    /// The name stored in the setting.
    pub fn key(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|environment| environment.key() == key)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Appearance {
    /// `None` keeps the driver's colour.
    pub color: Option<ConnectionColor>,
    pub environment: Option<Environment>,
}

impl Appearance {
    /// Reads a stored value; anything unreadable is the default, so a
    /// colour never stops a connection from opening.
    pub fn from_value(value: Option<&Value>) -> Self {
        let Some(Value::Object(fields)) = value else {
            return Self::default();
        };
        let text = |name: &str| fields.get(name).and_then(Value::as_str);
        let color = text("color").map(ConnectionColor::from_key);
        let environment = text("environment").map(Environment::from_key);
        // An unknown name means a value this version cannot read.
        if color == Some(None) || environment == Some(None) {
            return Self::default();
        }
        Self {
            color: color.flatten(),
            environment: environment.flatten(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut fields = serde_json::Map::new();
        if let Some(color) = self.color {
            fields.insert("color".into(), color.key().into());
        }
        if let Some(environment) = self.environment {
            fields.insert("environment".into(), environment.key().into());
        }
        Value::Object(fields)
    }

    pub fn is_production(&self) -> bool {
        self.environment == Some(Environment::Production)
    }
}

/// The setting that holds a connection's appearance.
pub fn appearance_key(id: ConnectionId) -> String {
    format!("connection.{id}.appearance")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_round_trips_and_reads_leniently() {
        let appearance = Appearance {
            color: Some(ConnectionColor::Red),
            environment: Some(Environment::Production),
        };
        assert_eq!(
            appearance.to_value(),
            serde_json::json!({"color": "red", "environment": "production"})
        );
        assert_eq!(
            Appearance::from_value(Some(&appearance.to_value())),
            appearance
        );
        assert!(appearance.is_production());
        // Nothing stored, an unknown colour or another shape: the default.
        assert_eq!(Appearance::from_value(None), Appearance::default());
        assert_eq!(
            Appearance::from_value(Some(&serde_json::json!({"color": "chartreuse"}))),
            Appearance::default()
        );
        assert_eq!(
            Appearance::from_value(Some(&serde_json::json!({"environment": "staging"}))),
            Appearance {
                color: None,
                environment: Some(Environment::Staging)
            }
        );
        assert_eq!(Appearance::default().to_value(), serde_json::json!({}));
    }
}
