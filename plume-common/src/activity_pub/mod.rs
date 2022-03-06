use activitypub::{Activity, Link, Object};
use activitystreams::{
    actor::{ApActor, Group, Person},
    base::{AnyBase, Base, Extends},
    iri_string::types::IriString,
    kind,
    markers::{self, Activity as Activity07},
    object::{ApObject, Article, Object as Object07},
    primitives::{AnyString, OneOrMany},
    unparsed::UnparsedMutExt,
};
use activitystreams_ext::{Ext1, Ext2, UnparsedExtension};
use array_tool::vec::Uniq;
use reqwest::{header::HeaderValue, r#async::ClientBuilder, Url};
use rocket::{
    http::Status,
    request::{FromRequest, Request},
    response::{Responder, Response},
    Outcome,
};
use tokio::prelude::*;
use tracing::{debug, warn};

use self::sign::Signable;

pub mod inbox;
pub mod request;
pub mod sign;

pub const CONTEXT_URL: &str = "https://www.w3.org/ns/activitystreams";
pub const PUBLIC_VISIBILITY: &str = "https://www.w3.org/ns/activitystreams#Public";

pub const AP_CONTENT_TYPE: &str =
    r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#;

pub fn ap_accept_header() -> Vec<&'static str> {
    vec![
        "application/ld+json; profile=\"https://w3.org/ns/activitystreams\"",
        "application/ld+json;profile=\"https://w3.org/ns/activitystreams\"",
        "application/activity+json",
        "application/ld+json",
    ]
}

pub fn context() -> serde_json::Value {
    json!([
        CONTEXT_URL,
        "https://w3id.org/security/v1",
        {
            "manuallyApprovesFollowers": "as:manuallyApprovesFollowers",
            "sensitive": "as:sensitive",
            "movedTo": "as:movedTo",
            "Hashtag": "as:Hashtag",
            "ostatus":"http://ostatus.org#",
            "atomUri":"ostatus:atomUri",
            "inReplyToAtomUri":"ostatus:inReplyToAtomUri",
            "conversation":"ostatus:conversation",
            "toot":"http://joinmastodon.org/ns#",
            "Emoji":"toot:Emoji",
            "focalPoint": {
                "@container":"@list",
                "@id":"toot:focalPoint"
            },
            "featured":"toot:featured"
        }
    ])
}

pub struct ActivityStream<T>(T);

impl<T> ActivityStream<T> {
    pub fn new(t: T) -> ActivityStream<T> {
        ActivityStream(t)
    }
}

impl<'r, O: Object> Responder<'r> for ActivityStream<O> {
    fn respond_to(self, request: &Request<'_>) -> Result<Response<'r>, Status> {
        let mut json = serde_json::to_value(&self.0).map_err(|_| Status::InternalServerError)?;
        json["@context"] = context();
        serde_json::to_string(&json).respond_to(request).map(|r| {
            Response::build_from(r)
                .raw_header("Content-Type", "application/activity+json")
                .finalize()
        })
    }
}

#[derive(Clone)]
pub struct ApRequest;
impl<'a, 'r> FromRequest<'a, 'r> for ApRequest {
    type Error = ();

    fn from_request(request: &'a Request<'r>) -> Outcome<Self, (Status, Self::Error), ()> {
        request
            .headers()
            .get_one("Accept")
            .map(|header| {
                header
                    .split(',')
                    .map(|ct| match ct.trim() {
                        // bool for Forward: true if found a valid Content-Type for Plume first (HTML), false otherwise
                        "application/ld+json; profile=\"https://w3.org/ns/activitystreams\""
                        | "application/ld+json;profile=\"https://w3.org/ns/activitystreams\""
                        | "application/activity+json"
                        | "application/ld+json" => Outcome::Success(ApRequest),
                        "text/html" => Outcome::Forward(true),
                        _ => Outcome::Forward(false),
                    })
                    .fold(Outcome::Forward(false), |out, ct| {
                        if out.clone().forwarded().unwrap_or_else(|| out.is_success()) {
                            out
                        } else {
                            ct
                        }
                    })
                    .map_forward(|_| ())
            })
            .unwrap_or(Outcome::Forward(()))
    }
}
pub fn broadcast<S, A, T, C>(sender: &S, act: A, to: Vec<T>, proxy: Option<reqwest::Proxy>)
where
    S: sign::Signer,
    A: Activity,
    T: inbox::AsActor<C>,
{
    let boxes = to
        .into_iter()
        .filter(|u| !u.is_local())
        .map(|u| {
            u.get_shared_inbox_url()
                .unwrap_or_else(|| u.get_inbox_url())
        })
        .collect::<Vec<String>>()
        .unique();

    let mut act = serde_json::to_value(act).expect("activity_pub::broadcast: serialization error");
    act["@context"] = context();
    let signed = act
        .sign(sender)
        .expect("activity_pub::broadcast: signature error");

    let mut rt = tokio::runtime::current_thread::Runtime::new()
        .expect("Error while initializing tokio runtime for federation");
    for inbox in boxes {
        let body = signed.to_string();
        let mut headers = request::headers();
        let url = Url::parse(&inbox);
        if url.is_err() {
            warn!("Inbox is invalid URL: {:?}", &inbox);
            continue;
        }
        let url = url.unwrap();
        if !url.has_host() {
            warn!("Inbox doesn't have host: {:?}", &inbox);
            continue;
        };
        let host_header_value = HeaderValue::from_str(url.host_str().expect("Unreachable"));
        if host_header_value.is_err() {
            warn!("Header value is invalid: {:?}", url.host_str());
            continue;
        }
        headers.insert("Host", host_header_value.unwrap());
        headers.insert("Digest", request::Digest::digest(&body));
        rt.spawn(
            if let Some(proxy) = proxy.clone() {
                ClientBuilder::new().proxy(proxy)
            } else {
                ClientBuilder::new()
            }
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Can't build client")
            .post(&inbox)
            .headers(headers.clone())
            .header(
                "Signature",
                request::signature(sender, &headers, ("post", url.path(), url.query()))
                    .expect("activity_pub::broadcast: request signature error"),
            )
            .body(body)
            .send()
            .and_then(move |r| {
                if r.status().is_success() {
                    debug!("Successfully sent activity to inbox ({})", &inbox);
                } else {
                    warn!("Error while sending to inbox ({:?})", &r)
                }
                r.into_body().concat2()
            })
            .map(move |response| debug!("Response: \"{:?}\"\n", response))
            .map_err(|e| warn!("Error while sending to inbox ({:?})", e)),
        );
    }
    rt.run().unwrap();
}

pub fn broadcast07<S, T, A, K, C>(sender: &S, act: A, to: Vec<T>, proxy: Option<reqwest::Proxy>)
where
    S: sign::Signer,
    A: Activity07 + serde::Serialize,
    T: inbox::AsActor<C>,
{
    let boxes = to
        .into_iter()
        .map(|u| {
            u.get_shared_inbox_url()
                .unwrap_or_else(|| u.get_inbox_url())
        })
        .collect::<Vec<String>>()
        .unique();

    let mut act = serde_json::to_value(act).expect("activity_pub::broadcast: serialization error");
    act["@context"] = context();
    let signed = act
        .sign(sender)
        .expect("activity_pub::broadcast: signature error");

    let mut rt = tokio::runtime::current_thread::Runtime::new()
        .expect("Error while initializing tokio runtime for federation");
    for inbox in boxes {
        let body = signed.to_string();
        let mut headers = request::headers();
        let url = Url::parse(&inbox);
        if url.is_err() {
            warn!("Inbox is invalid URL: {:?}", &inbox);
            continue;
        }
        let url = url.unwrap();
        if !url.has_host() {
            warn!("Inbox doesn't have host: {:?}", &inbox);
            continue;
        };
        let host_header_value = HeaderValue::from_str(url.host_str().expect("Unreachable"));
        if host_header_value.is_err() {
            warn!("Header value is invalid: {:?}", url.host_str());
            continue;
        }
        headers.insert("Host", host_header_value.unwrap());
        headers.insert("Digest", request::Digest::digest(&body));
        rt.spawn(
            if let Some(proxy) = proxy.clone() {
                ClientBuilder::new().proxy(proxy)
            } else {
                ClientBuilder::new()
            }
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Can't build client")
            .post(&inbox)
            .headers(headers.clone())
            .header(
                "Signature",
                request::signature(sender, &headers, ("post", url.path(), url.query()))
                    .expect("activity_pub::broadcast: request signature error"),
            )
            .body(body)
            .send()
            .and_then(move |r| {
                if r.status().is_success() {
                    debug!("Successfully sent activity to inbox ({})", &inbox);
                } else {
                    warn!("Error while sending to inbox ({:?})", &r)
                }
                r.into_body().concat2()
            })
            .map(move |response| debug!("Response: \"{:?}\"\n", response))
            .map_err(|e| warn!("Error while sending to inbox ({:?})", e)),
        );
    }
    rt.run().unwrap();
}

#[derive(Shrinkwrap, Clone, Serialize, Deserialize)]
pub struct Id(String);

impl Id {
    pub fn new(id: impl ToString) -> Id {
        Id(id.to_string())
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

pub trait IntoId {
    fn into_id(self) -> Id;
}

impl Link for Id {}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Properties)]
#[serde(rename_all = "camelCase")]
pub struct ApSignature {
    #[activitystreams(concrete(PublicKey), functional)]
    pub public_key: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Properties)]
#[serde(rename_all = "camelCase")]
pub struct PublicKey {
    #[activitystreams(concrete(String), functional)]
    pub id: Option<serde_json::Value>,

    #[activitystreams(concrete(String), functional)]
    pub owner: Option<serde_json::Value>,

    #[activitystreams(concrete(String), functional)]
    pub public_key_pem: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApSignature07 {
    pub public_key: PublicKey07,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublicKey07 {
    pub id: IriString,
    pub owner: IriString,
    pub public_key_pem: String,
}

impl<U> UnparsedExtension<U> for ApSignature07
where
    U: UnparsedMutExt,
{
    type Error = serde_json::Error;

    fn try_from_unparsed(unparsed_mut: &mut U) -> Result<Self, Self::Error> {
        Ok(ApSignature07 {
            public_key: unparsed_mut.remove("publicKey")?,
        })
    }

    fn try_into_unparsed(self, unparsed_mut: &mut U) -> Result<(), Self::Error> {
        unparsed_mut.insert("publicKey", self.public_key)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActorSource {
    pub source: Source,
}

impl<U> UnparsedExtension<U> for ActorSource
where
    U: UnparsedMutExt,
{
    type Error = serde_json::Error;

    fn try_from_unparsed(unparsed_mut: &mut U) -> Result<Self, Self::Error> {
        Ok(ActorSource {
            source: unparsed_mut.remove("source")?,
        })
    }

    fn try_into_unparsed(self, unparsed_mut: &mut U) -> Result<(), Self::Error> {
        unparsed_mut.insert("source", self.source)?;
        Ok(())
    }
}

pub type CustomPerson = Ext1<ApActor<Person>, ApSignature07>;
pub type CustomGroup = Ext2<ApActor<Group>, ApSignature07, ActorSource>;

#[derive(Clone, Debug, Default, UnitString)]
#[activitystreams(Hashtag)]
pub struct HashtagType;

#[derive(Clone, Debug, Default, Deserialize, Serialize, Properties)]
#[serde(rename_all = "camelCase")]
pub struct Hashtag {
    #[serde(rename = "type")]
    kind: HashtagType,

    #[activitystreams(concrete(String), functional)]
    pub href: Option<serde_json::Value>,

    #[activitystreams(concrete(String), functional)]
    pub name: Option<serde_json::Value>,
}

kind!(HashtagType07, Hashtag);

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Hashtag07 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<IriString>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<AnyString>,

    #[serde(flatten)]
    inner: Object07<HashtagType07>,
}

impl Hashtag07 {
    pub fn new() -> Self {
        Hashtag07 {
            href: None,
            name: None,
            inner: Object07::new(),
        }
    }

    pub fn extending(mut inner: Object07<HashtagType07>) -> Result<Self, serde_json::Error> {
        let href = inner.remove("href")?;
        let name = inner.remove("name")?;

        Ok(Hashtag07 { href, name, inner })
    }

    pub fn retracting(self) -> Result<Object07<HashtagType07>, serde_json::Error> {
        let Hashtag07 {
            href,
            name,
            mut inner,
        } = self;

        inner.insert("href", href)?;
        inner.insert("name", name)?;
        Ok(inner)
    }
}

pub trait AsHashtag: markers::Object {
    fn hashtag_ref(&self) -> &Hashtag07;

    fn hashtag_mut(&mut self) -> &mut Hashtag07;
}

pub trait HashtagExt: AsHashtag {
    fn href(&self) -> Option<&IriString> {
        self.hashtag_ref().href.as_ref()
    }

    fn set_href<T>(&mut self, href: T) -> &mut Self
    where
        T: Into<IriString>,
    {
        self.hashtag_mut().href = Some(href.into());
        self
    }

    fn take_href(&mut self) -> Option<IriString> {
        self.hashtag_mut().href.take()
    }

    fn delete_href(&mut self) -> &mut Self {
        self.hashtag_mut().href = None;
        self
    }

    fn name(&self) -> Option<&AnyString> {
        self.hashtag_ref().name.as_ref()
    }

    fn set_name<T>(&mut self, name: T) -> &mut Self
    where
        T: Into<AnyString>,
    {
        self.hashtag_mut().name = Some(name.into());
        self
    }

    fn take_name(&mut self) -> Option<AnyString> {
        self.hashtag_mut().name.take()
    }

    fn delete_name(&mut self) -> &mut Self {
        self.hashtag_mut().name = None;
        self
    }
}

impl AsHashtag for Hashtag07 {
    fn hashtag_ref(&self) -> &Hashtag07 {
        self
    }

    fn hashtag_mut(&mut self) -> &mut Hashtag07 {
        self
    }
}

impl Extends<HashtagType07> for Hashtag07 {
    type Error = serde_json::Error;

    fn extends(base: Base<HashtagType07>) -> Result<Self, Self::Error> {
        let inner = Object07::extends(base)?;
        Self::extending(inner)
    }

    fn retracts(self) -> Result<Base<HashtagType07>, Self::Error> {
        let inner = self.retracting()?;
        inner.retracts()
    }
}

impl markers::Base for Hashtag07 {}
impl markers::Object for Hashtag07 {}
impl<T> HashtagExt for T where T: AsHashtag {}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub media_type: String,

    pub content: String,
}

impl Object for Source {}

impl<U> UnparsedExtension<U> for Source
where
    U: UnparsedMutExt,
{
    type Error = serde_json::Error;

    fn try_from_unparsed(unparsed_mut: &mut U) -> Result<Self, Self::Error> {
        Ok(Source {
            content: unparsed_mut.remove("content")?,
            media_type: unparsed_mut.remove("mediaType")?,
        })
    }

    fn try_into_unparsed(self, unparsed_mut: &mut U) -> Result<(), Self::Error> {
        unparsed_mut.insert("content", self.content)?;
        unparsed_mut.insert("mediaType", self.media_type)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Properties)]
#[serde(rename_all = "camelCase")]
pub struct Licensed {
    #[activitystreams(concrete(String), functional)]
    pub license: Option<serde_json::Value>,
}

impl Object for Licensed {}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Licensed07 {
    pub license: String,
}

impl<U> UnparsedExtension<U> for Licensed07
where
    U: UnparsedMutExt,
{
    type Error = serde_json::Error;

    fn try_from_unparsed(unparsed_mut: &mut U) -> Result<Self, Self::Error> {
        Ok(Licensed07 {
            license: unparsed_mut.remove("license")?,
        })
    }

    fn try_into_unparsed(self, unparsed_mut: &mut U) -> Result<(), Self::Error> {
        unparsed_mut.insert("license", self.license)?;
        Ok(())
    }
}

pub type LicensedArticle = Ext1<ApObject<Article>, Licensed07>;

pub trait ToAsString {
    fn to_as_string(&self) -> Option<String>;
}

impl ToAsString for OneOrMany<&AnyString> {
    fn to_as_string(&self) -> Option<String> {
        self.as_as_str().map(|s| s.to_string())
    }
}

trait AsAsStr {
    fn as_as_str(&self) -> Option<&str>;
}

impl AsAsStr for OneOrMany<&AnyString> {
    fn as_as_str(&self) -> Option<&str> {
        self.iter().next().map(|prop| prop.as_str())
    }
}

pub trait ToAsUri {
    fn to_as_uri(&self) -> Option<String>;
}

impl ToAsUri for OneOrMany<AnyBase> {
    fn to_as_uri(&self) -> Option<String> {
        self.iter()
            .next()
            .and_then(|prop| prop.as_xsd_any_uri().map(|uri| uri.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_json_diff::assert_json_eq;
    use serde_json::{from_str, json, to_value};

    #[test]
    fn se_ap_signature() {
        let ap_signature = ApSignature07 {
            public_key: PublicKey07 {
                id: "https://example.com/pubkey".parse().unwrap(),
                owner: "https://example.com/owner".parse().unwrap(),
                public_key_pem: "pubKeyPem".into(),
            },
        };
        let expected = json!({
            "publicKey": {
                "id": "https://example.com/pubkey",
                "owner": "https://example.com/owner",
                "publicKeyPem": "pubKeyPem"
            }
        });
        assert_json_eq!(to_value(ap_signature).unwrap(), expected);
    }

    #[test]
    fn de_ap_signature() {
        let value: ApSignature07 = from_str(
            r#"
              {
                "publicKey": {
                  "id": "https://example.com/",
                  "owner": "https://example.com/",
                  "publicKeyPem": ""
                }
              }
            "#,
        )
        .unwrap();
        let expected = ApSignature07 {
            public_key: PublicKey07 {
                id: "https://example.com/".parse().unwrap(),
                owner: "https://example.com/".parse().unwrap(),
                public_key_pem: "".into(),
            },
        };
        assert_eq!(value, expected);
    }

    #[test]
    fn se_custom_person() {
        let actor = ApActor::new("https://example.com/inbox".parse().unwrap(), Person::new());
        let person = CustomPerson::new(
            actor,
            ApSignature07 {
                public_key: PublicKey07 {
                    id: "https://example.com/pubkey".parse().unwrap(),
                    owner: "https://example.com/owner".parse().unwrap(),
                    public_key_pem: "pubKeyPem".into(),
                },
            },
        );
        let expected = json!({
            "inbox": "https://example.com/inbox",
            "type": "Person",
            "publicKey": {
                "id": "https://example.com/pubkey",
                "owner": "https://example.com/owner",
                "publicKeyPem": "pubKeyPem"
            }
        });
        assert_eq!(to_value(person).unwrap(), expected);
    }

    #[test]
    fn se_licensed_article() {
        let object = ApObject::new(Article::new());
        let licensed_article = LicensedArticle::new(
            object,
            Licensed07 {
                license: "CC-0".into(),
            },
        );
        let expected = json!({
            "type": "Article",
            "license": "CC-0"
        });
        assert_json_eq!(to_value(licensed_article).unwrap(), expected);
    }
}
