use std::{
    fmt,
    time::{
        Duration,
        SystemTime,
    },
};

use anyhow::Context;
use common::{
    identity::{
        IdentityCacheKey,
        InertIdentity,
    },
    index::IndexKeyBytes,
    query::{
        Cursor,
        CursorPosition,
        SerializedCursor,
    },
    query_journal::QueryJournal,
    runtime::{
        Runtime,
        UnixTimestamp,
    },
    types::{
        format_admin_key,
        remove_type_prefix_from_instance_name,
        split_admin_key,
        ActionCallbackToken,
        AdminKey,
        MemberId,
        PersistenceVersion,
    },
};
use errors::ErrorMetadata;
use openidconnect::{
    core::{
        CoreIdToken,
        CoreIdTokenVerifier,
    },
    Nonce,
};
use pb::{
    convex_actions::ActionCallbackToken as ActionCallbackTokenProto,
    convex_cursor::{
        cursor::Position as PositionProto,
        Cursor as CursorProto,
        IndexKey as IndexKeyProto,
    },
    convex_identity::{
        identity::Identity as IdentityProto,
        ActingUser,
    },
    convex_keys::{
        admin_key::Identity as AdminIdentityProto,
        storage_token::{
            AuthorizationType as AuthorizationTypeProto,
            StoreFile as StoreFileProto,
        },
        AdminKey as AdminKeyProto,
        StorageToken as StorageTokenProto,
    },
    convex_query_journal::QueryJournal as QueryJournalProto,
};
#[cfg(any(test, feature = "testing"))]
use proptest::prelude::{
    Arbitrary,
    Strategy,
};
use sync_types::{
    AuthenticationToken,
    SerializedQueryJournal,
    UserIdentifier,
    UserIdentityAttributes,
};

#[cfg(any(test, feature = "testing"))]
use crate::testing::TestUserIdentity;
use crate::{
    encryptor::Encryptor,
    metrics::{
        log_actions_token_expired,
        log_store_file_auth_expired,
    },
    secret::InstanceSecret,
};

const ACTION_KEY_VERSION: u8 = 1;
const ADMIN_KEY_VERSION: u8 = 1;
const CURSOR_VERSION: u8 = 7;
const STORE_FILE_AUTHZ_VERSION: u8 = 1;
const QUERY_JOURNAL_VERSION: u8 = 7;

// Max delay from transaction start time -> key being issued that is tolerable.
const MAX_TS_DELAY: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct KeyBroker {
    instance_name: String,
    encryptor: Encryptor,
}

// This enum encodes a successful authentication decision, and its nontrivial
// variants cannot be constructed outside this crate. Since possession of this
// value confers access permissions, don't store it persistently: Instead, use
// [`common::identity::InertIdentity`] to store an "inert" version that records
// the variant without representation authentication.
#[derive(Clone, Debug)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum Identity {
    InstanceAdmin(AdminIdentity),
    System(SystemIdentity),
    User(UserIdentity),
    // ActingUser keeps track of the ID of the admin acting as a user,
    // and that user's fake attributes
    ActingUser(AdminIdentity, UserIdentityAttributes),
    Unknown,
}

impl From<Identity> for AuthenticationToken {
    fn from(i: Identity) -> Self {
        match i {
            Identity::User(identity) => {
                AuthenticationToken::User(identity.original_token.to_string())
            },
            Identity::ActingUser(identity, user) => {
                AuthenticationToken::Admin(identity.key, Some(user))
            },
            Identity::InstanceAdmin(identity) => AuthenticationToken::Admin(identity.key, None),
            _ => AuthenticationToken::None,
        }
    }
}

impl From<Identity> for pb::convex_identity::Identity {
    fn from(i: Identity) -> Self {
        let identity = match i {
            Identity::InstanceAdmin(admin_identity) => {
                IdentityProto::AdminIdentity(admin_identity.into())
            },
            Identity::System(_) => IdentityProto::System(()),
            Identity::User(user_identity) => IdentityProto::UserIdentity(user_identity.into()),
            Identity::ActingUser(admin_identity, attributes) => {
                IdentityProto::ActingUser(ActingUser {
                    admin_identity: Some(admin_identity.into()),
                    attributes: Some(attributes.into()),
                })
            },
            Identity::Unknown => IdentityProto::Unknown(()),
        };
        Self {
            identity: Some(identity),
        }
    }
}

impl Identity {
    pub fn from_proto_unchecked(msg: pb::convex_identity::Identity) -> anyhow::Result<Self> {
        let identity = msg
            .identity
            .ok_or_else(|| anyhow::anyhow!("Missing nested identity"))?;
        match identity {
            IdentityProto::AdminIdentity(admin_identity) => Ok(Identity::InstanceAdmin(
                AdminIdentity::from_proto_unchecked(admin_identity)?,
            )),
            IdentityProto::System(()) => Ok(Identity::System(SystemIdentity)),
            IdentityProto::UserIdentity(user_identity) => Ok(Identity::User(
                UserIdentity::from_proto_unchecked(user_identity)?,
            )),
            IdentityProto::ActingUser(ActingUser {
                admin_identity,
                attributes,
            }) => {
                let admin_identity = AdminIdentity::from_proto_unchecked(
                    admin_identity.ok_or_else(|| anyhow::anyhow!("Missing admin identity"))?,
                )?;
                let attributes =
                    attributes.ok_or_else(|| anyhow::anyhow!("Missing user attributes"))?;
                Ok(Identity::ActingUser(admin_identity, attributes.try_into()?))
            },
            IdentityProto::Unknown(()) => Ok(Identity::Unknown),
        }
    }
}

impl From<Identity> for InertIdentity {
    fn from(i: Identity) -> Self {
        match i {
            Identity::InstanceAdmin(i) => InertIdentity::InstanceAdmin(i.instance_name),
            Identity::System(_) => InertIdentity::System,
            Identity::Unknown => InertIdentity::Unknown,
            Identity::User(user) => InertIdentity::User(user.attributes.token_identifier),
            Identity::ActingUser(identity, user) => {
                InertIdentity::ActingUser(identity.member_id, user.token_identifier)
            },
        }
    }
}

impl PartialEq for Identity {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InstanceAdmin(l), Self::InstanceAdmin(r)) => l == r,
            (Self::System(..), Self::System(..)) => true,
            (Self::User(l), Self::User(r)) => {
                l.attributes.token_identifier == r.attributes.token_identifier
            },
            (Self::Unknown, Self::Unknown) => true,
            (
                Self::ActingUser(l_admin_identity, l_attributes),
                Self::ActingUser(r_admin_identity, r_attributes),
            ) => l_admin_identity == r_admin_identity && l_attributes == r_attributes,
            (Self::InstanceAdmin(_), _)
            | (Self::System(_), _)
            | (Self::User(_), _)
            | (Self::Unknown, _)
            | (Self::ActingUser(..), _) => false,
        }
    }
}

impl Eq for Identity {}

impl Identity {
    pub fn cache_key(&self) -> IdentityCacheKey {
        match self.clone() {
            Identity::InstanceAdmin(i) => IdentityCacheKey::InstanceAdmin(i.instance_name),
            Identity::System(_) => IdentityCacheKey::System,
            Identity::Unknown => IdentityCacheKey::Unknown,
            Identity::User(user) => IdentityCacheKey::User(user.attributes),
            Identity::ActingUser(identity, user) => {
                IdentityCacheKey::ActingUser(identity.member_id, user)
            },
        }
    }

    /// Easy-to-audit entry point for creating a global system identity.
    pub fn system() -> Self {
        Identity::System(SystemIdentity)
    }

    /// Entry point for creating a User identity after authenticating the user.
    pub fn user(user: UserIdentity) -> Self {
        Identity::User(user)
    }

    pub fn is_system(&self) -> bool {
        matches!(self, Identity::System(..))
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Identity::InstanceAdmin(..))
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Identity::User(..))
    }

    /// Returns the admin's [`MemberId`] if this is an
    /// [`Identity::InstanceAdmin`]
    pub fn member_id(&self) -> Option<MemberId> {
        if let &Identity::InstanceAdmin(AdminIdentity { member_id, .. }) = self {
            return Some(member_id);
        }
        None
    }

    pub fn instance_name(&self) -> Option<String> {
        if let Identity::InstanceAdmin(AdminIdentity { instance_name, .. }) = self {
            return Some(instance_name.to_string());
        }
        None
    }

    pub fn user_identity(&self) -> Option<UserIdentity> {
        if let Identity::User(id) = self {
            return Some(id.clone());
        }
        None
    }

    pub fn assert_present(&self) -> anyhow::Result<()> {
        if *self == Identity::Unknown {
            anyhow::bail!(ErrorMetadata::unauthenticated(
                "AuthorizationMissing",
                "This request requires the HTTP `Authorization` header.",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive] // Prevents creating this struct without calling `from_token`
pub struct UserIdentity {
    pub subject: String,
    // Might be useful for developers to know which provider authenticated this user.
    pub issuer: String,
    pub expiration: SystemTime,
    pub attributes: UserIdentityAttributes,
    // The original token this user identity was created from.
    pub original_token: CoreIdToken,
}

#[cfg(any(test, feature = "testing"))]
impl Arbitrary for UserIdentity {
    type Parameters = ();

    type Strategy = impl Strategy<Value = UserIdentity>;

    fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
        use proptest::prelude::*;

        // This is not randomized right now because there are many constraints on
        // string fields in UserIdentity.
        any::<()>().prop_map(|()| UserIdentity::test())
    }
}

impl From<UserIdentity> for pb::convex_identity::UserIdentity {
    fn from(
        UserIdentity {
            subject,
            issuer,
            expiration,
            attributes,
            original_token,
        }: UserIdentity,
    ) -> Self {
        Self {
            subject: Some(subject),
            issuer: Some(issuer),
            expiration: Some(expiration.into()),
            attributes: Some(attributes.into()),
            original_token: Some(original_token.to_string()),
        }
    }
}

macro_rules! get_string {
    ($claims: ident, $field: ident) => {
        $claims.$field().map(|v| v.to_string())
    };
}
macro_rules! get_localized_string {
    ($claims: ident, $field: ident) => {
        $claims
            .$field()
            .and_then(|loc| loc.get(None))
            .map(|v| v.to_string())
    };
}

impl UserIdentity {
    pub fn from_token(
        token: CoreIdToken,
        verifier: CoreIdTokenVerifier,
    ) -> Result<Self, anyhow::Error> {
        // NB: Nonce verification is optional, and we'd need the developer to create and
        // store a nonce with their initial request to auth0 (or whatever
        // provider they use) and provide that nonce to us with the ID token.
        let nonce_verifier = |_: Option<&Nonce>| Ok(());
        let binding = token.clone();
        let claims = binding.claims(&verifier, nonce_verifier)?;
        let subject = claims.subject().to_string();
        let issuer = claims.issuer().to_string();
        Ok(UserIdentity {
            subject: subject.clone(),
            issuer: issuer.clone(),
            expiration: claims.expiration().into(),
            original_token: token,
            attributes: UserIdentityAttributes {
                token_identifier: UserIdentifier::construct(&issuer, &subject),
                subject: Some(subject),
                issuer: Some(issuer),
                name: get_localized_string!(claims, name),
                given_name: get_localized_string!(claims, given_name),
                family_name: get_localized_string!(claims, family_name),
                nickname: get_localized_string!(claims, nickname),
                preferred_username: get_string!(claims, preferred_username),
                profile_url: get_localized_string!(claims, profile),
                picture_url: get_localized_string!(claims, picture),
                website_url: get_localized_string!(claims, website),
                email: get_string!(claims, email),
                email_verified: claims.email_verified(),
                gender: get_string!(claims, gender),
                birthday: get_string!(claims, birthday),
                timezone: get_string!(claims, zoneinfo),
                language: get_string!(claims, locale),
                phone_number: get_string!(claims, phone_number),
                phone_number_verified: claims.phone_number_verified(),
                address: claims
                    .address()
                    .and_then(|a| a.formatted.as_ref())
                    .map(|f| f.to_string()),
                updated_at: claims.updated_at().map(|dt| dt.to_rfc3339()),
            },
        })
    }

    // Decode an `Identity` serialized to protobuf *without* revalidating its
    // original token. This method assumes that the protobuf comes from a
    // trusted source, like an internal backend.
    pub fn from_proto_unchecked(msg: pb::convex_identity::UserIdentity) -> anyhow::Result<Self> {
        let subject = msg
            .subject
            .ok_or_else(|| anyhow::anyhow!("Missing subject"))?;
        let issuer = msg
            .issuer
            .ok_or_else(|| anyhow::anyhow!("Missing issuer"))?;
        let expiration = msg
            .expiration
            .ok_or_else(|| anyhow::anyhow!("Missing expiration"))?
            .try_into()?;
        let attributes = msg
            .attributes
            .ok_or_else(|| anyhow::anyhow!("Missing user identity attributes"))?
            .try_into()?;
        let original_token = msg
            .original_token
            .ok_or_else(|| anyhow::anyhow!("Missing original_token"))?
            .parse()?;
        Ok(Self {
            subject,
            issuer,
            expiration,
            attributes,
            original_token,
        })
    }

    pub fn is_expired(&self, current_time: SystemTime) -> bool {
        current_time >= self.expiration
    }
}

// Token indicating the possessor has authenticated as the admin for an
// instance.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AdminIdentity {
    instance_name: String,
    member_id: MemberId,
    key: String,
}

impl From<AdminIdentity> for pb::convex_identity::AdminIdentity {
    fn from(
        AdminIdentity {
            instance_name,
            member_id,
            key,
        }: AdminIdentity,
    ) -> Self {
        Self {
            instance_name: Some(instance_name),
            member_id: Some(member_id.0),
            key: Some(key),
        }
    }
}

impl AdminIdentity {
    pub fn from_proto_unchecked(msg: pb::convex_identity::AdminIdentity) -> anyhow::Result<Self> {
        let instance_name = msg
            .instance_name
            .ok_or_else(|| anyhow::anyhow!("Missing instance_name"))?;
        let member_id = msg
            .member_id
            .ok_or_else(|| anyhow::anyhow!("Missing member_id"))?;
        let key = msg.key.ok_or_else(|| anyhow::anyhow!("Missing key"))?;
        Ok(Self {
            instance_name,
            member_id: MemberId(member_id),
            key,
        })
    }
}

#[cfg(any(test, feature = "testing"))]
impl Arbitrary for AdminIdentity {
    type Parameters = ();

    type Strategy = impl proptest::strategy::Strategy<Value = AdminIdentity>;

    fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
        use proptest::prelude::*;
        any::<(u64, String)>().prop_map(|(member_id, key)| AdminIdentity {
            instance_name: "fake-instance-name".to_string(),
            member_id: MemberId(member_id),
            key,
        })
    }
}

#[cfg(any(test, feature = "testing"))]
impl AdminIdentity {
    pub fn new_for_test_only(instance_name: String, member_id: MemberId) -> AdminIdentity {
        AdminIdentity {
            instance_name,
            member_id,
            key: "chocolate-charlies-cupcake".to_string(),
        }
    }

    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }
}

impl fmt::Debug for AdminIdentity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}/{}", self.instance_name, self.member_id, self.key)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub struct SystemIdentity;
impl fmt::Debug for SystemIdentity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SystemIdentity")
    }
}

/// Encrypted system key
#[derive(derive_more::Display)]
pub struct SystemKey(String);
/// Encrypted authorization to store a file
#[derive(Debug, derive_more::Display)]
pub struct StoreFileAuthorization(String);
/// Encrypted authorization to get a file
#[derive(Debug, derive_more::Display)]
pub struct GetFileAuthorization(String);

pub fn cursor_parse_error() -> ErrorMetadata {
    ErrorMetadata::bad_request("InvalidCursor", "Failed to parse cursor")
}

impl KeyBroker {
    pub fn new(instance_name: &str, instance_secret: InstanceSecret) -> anyhow::Result<Self> {
        Ok(Self {
            instance_name: instance_name.to_owned(),
            encryptor: Encryptor::new(instance_secret)?,
        })
    }

    pub fn dev() -> Self {
        Self::new(
            crate::DEV_INSTANCE_NAME,
            InstanceSecret::try_from(crate::DEV_SECRET).unwrap(),
        )
        .unwrap()
    }

    pub fn issue_admin_key(&self, member_id: MemberId) -> AdminKey {
        AdminKey::new(self.issue_key(Some(member_id)))
    }

    pub fn issue_system_key(&self) -> SystemKey {
        SystemKey(self.issue_key(None))
    }

    pub fn issue_store_file_authorization<RT: Runtime>(
        &self,
        rt: &RT,
        issued: UnixTimestamp,
    ) -> anyhow::Result<StoreFileAuthorization> {
        let now = rt.unix_timestamp();
        if (now - issued) > MAX_TS_DELAY {
            anyhow::bail!("Could not issue authorization. Issued TS too far in past.");
        }
        Ok(StoreFileAuthorization(self.encryptor.encode_proto(
            STORE_FILE_AUTHZ_VERSION,
            StorageTokenProto {
                instance_name: self.instance_name.clone(),
                issued_s: issued.as_secs(),
                authorization_type: Some(AuthorizationTypeProto::StoreFile(StoreFileProto {})),
            },
        )))
    }

    /// Private helper method to generate an admin key.
    /// If `member_id` is None, it generates a system key, otherwise
    /// an admin key for the given user.
    fn issue_key(&self, member_id: Option<MemberId>) -> String {
        let now = SystemTime::now();
        let since_epoch = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to compute seconds since epoch?");

        let identity = match member_id {
            Some(member_id) => AdminIdentityProto::MemberId(member_id.0),
            None => AdminIdentityProto::System(()),
        };
        let proto = AdminKeyProto {
            instance_name: None,
            issued_s: since_epoch.as_secs(),
            identity: Some(identity),
        };
        format_admin_key(
            &self.instance_name,
            &self.encryptor.encode_proto(ADMIN_KEY_VERSION, proto),
        )
    }

    pub fn check_admin_key(&self, key: &str) -> anyhow::Result<Identity> {
        let (instance_name, encrypted_part) = split_admin_key(key)
            .map(|(name, key)| (Some(remove_type_prefix_from_instance_name(name)), key))
            .unwrap_or((None, key));
        let AdminKeyProto {
            instance_name: instance_name_from_encrypted_part,
            issued_s,
            identity,
        } = self
            .encryptor
            .decode_proto(ADMIN_KEY_VERSION, encrypted_part)
            .with_context(|| format!("Couldn't decode the AdminKeyProto {}", key))?;
        let instance_name = instance_name
            .or(instance_name_from_encrypted_part.as_deref())
            .context("Invalid admin key format")?;

        if instance_name != self.instance_name {
            return Err(anyhow::anyhow!(
                "Key is for invalid instance {instance_name}",
            ));
        }
        anyhow::ensure!(issued_s != 0, "Proto missing issued_s");
        let identity = identity.context("Proto missing identity")?;

        Ok(match identity {
            AdminIdentityProto::MemberId(member_id) => Identity::InstanceAdmin(AdminIdentity {
                instance_name: self.instance_name.clone(),
                member_id: MemberId(member_id),
                key: key.to_string(),
            }),
            AdminIdentityProto::System(()) => Identity::system(),
        })
    }

    pub fn check_store_file_authorization<RT: Runtime>(
        &self,
        rt: &RT,
        store_file_authorization: &str,
        validity: Duration,
    ) -> anyhow::Result<()> {
        let StorageTokenProto {
            instance_name,
            issued_s,
            authorization_type,
        } = self
            .encryptor
            .decode_proto(STORE_FILE_AUTHZ_VERSION, store_file_authorization)
            .context(ErrorMetadata::unauthenticated(
                "StorageTokenInvalid",
                "Couldn't decode the StoreFileAuthorization token",
            ))?;

        if instance_name != self.instance_name {
            anyhow::bail!(ErrorMetadata::unauthenticated(
                "InvalidStorageToken",
                "Storage token is for invalid instance {instance_name}"
            ));
        }

        anyhow::ensure!(issued_s != 0, "Proto missing issued_s");
        let now = rt.unix_timestamp().as_secs();
        if issued_s + validity.as_secs() <= now {
            log_store_file_auth_expired();
            anyhow::bail!(ErrorMetadata::unauthenticated(
                "StorageTokenExpired",
                "Store File Authorization expired",
            ));
        }

        let Some(AuthorizationTypeProto::StoreFile(StoreFileProto {})) = authorization_type else {
            anyhow::bail!(ErrorMetadata::unauthenticated(
                "InvalidStorageToken",
                "Storage token is for invalid instance {instance_name}"
            ));
        };

        Ok(())
    }

    fn cursor_to_proto(&self, cursor: &Cursor) -> CursorProto {
        let position = match cursor.position {
            CursorPosition::End => PositionProto::End(()),
            CursorPosition::After(ref key) => PositionProto::After(IndexKeyProto {
                values: key.clone().0,
            }),
        };
        CursorProto {
            instance_name: self.instance_name.clone(),
            position: Some(position),
            query_fingerprint: cursor.query_fingerprint.clone(),
        }
    }

    fn proto_to_cursor(&self, proto: CursorProto) -> anyhow::Result<Cursor> {
        if proto.instance_name != self.instance_name {
            anyhow::bail!(ErrorMetadata::bad_request(
                "InvalidCursor",
                format!("Key is invalid for instance {:?}", proto.instance_name)
            ));
        }

        let cursor_position = match proto.position {
            Some(PositionProto::End(())) => CursorPosition::End,
            Some(PositionProto::After(IndexKeyProto {
                values: proto_values,
            })) => CursorPosition::After(IndexKeyBytes(proto_values)),
            None => anyhow::bail!(ErrorMetadata::bad_request(
                "InvalidCursor",
                "Missing position field"
            )),
        };
        Ok(Cursor {
            position: cursor_position,
            query_fingerprint: proto.query_fingerprint,
        })
    }

    /// Serializes and encrypts the provided Cursor for sending to clients.
    pub fn encrypt_cursor(
        &self,
        cursor: &Cursor,
        persistence_version: PersistenceVersion,
    ) -> SerializedCursor {
        let proto = self.cursor_to_proto(cursor);
        let cursor_version = persistence_version.index_key_version(CURSOR_VERSION);
        self.encryptor.encode_proto(cursor_version, proto)
    }

    /// Attempts to decrypt and deserialize the EncryptedCursor. May fail if the
    /// client is sending up an old version.
    pub fn decrypt_cursor(
        &self,
        cursor: SerializedCursor,
        persistence_version: PersistenceVersion,
    ) -> anyhow::Result<Cursor> {
        let cursor_version = persistence_version.index_key_version(CURSOR_VERSION);
        let proto: CursorProto = self
            .encryptor
            .decode_proto(cursor_version, &cursor)
            .with_context(cursor_parse_error)?;
        self.proto_to_cursor(proto)
    }

    pub fn encrypt_query_journal(
        &self,
        journal: &QueryJournal,
        persistence_version: PersistenceVersion,
    ) -> SerializedQueryJournal {
        let query_journal_version = persistence_version.index_key_version(QUERY_JOURNAL_VERSION);
        let cursor = match &journal.end_cursor {
            Some(cursor) => Some(self.cursor_to_proto(cursor)),
            None => return None,
        };
        let proto = QueryJournalProto { end_cursor: cursor };
        Some(self.encryptor.encode_proto(query_journal_version, proto))
    }

    pub fn decrypt_query_journal(
        &self,
        journal: SerializedQueryJournal,
        persistence_version: PersistenceVersion,
    ) -> anyhow::Result<QueryJournal> {
        let query_journal_version = persistence_version.index_key_version(QUERY_JOURNAL_VERSION);
        match journal {
            None => Ok(QueryJournal::new()),
            Some(journal) => {
                let proto: QueryJournalProto = self
                    .encryptor
                    .decode_proto(query_journal_version, &journal)
                    .with_context(cursor_parse_error)?;
                let end_cursor = match proto.end_cursor {
                    Some(cursor) => Some(self.proto_to_cursor(cursor)?),
                    None => None,
                };
                Ok(QueryJournal { end_cursor })
            },
        }
    }

    pub fn issue_action_token(&self) -> ActionCallbackToken {
        let now = SystemTime::now();
        let since_epoch = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to compute seconds since epoch?");

        let proto = ActionCallbackTokenProto {
            issued_s: since_epoch.as_secs(),
        };

        self.encryptor.encode_proto(ACTION_KEY_VERSION, proto)
    }

    // Checks the action token and returns its issue time.
    pub fn check_action_token(
        &self,
        token: &ActionCallbackToken,
        validity: Duration,
    ) -> anyhow::Result<SystemTime> {
        let ActionCallbackTokenProto { issued_s } = self
            .encryptor
            .decode_proto(ACTION_KEY_VERSION, token)
            .with_context(|| format!("Couldn't decode ActionCallbackTokenProto {token}"))?;

        anyhow::ensure!(issued_s != 0, "ActionCallbackTokenProto missing issued_s");

        let now = SystemTime::now();
        let since_epoch = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to compute seconds since epoch?")
            .as_secs();
        if issued_s + validity.as_secs() <= since_epoch {
            log_actions_token_expired();
            // Note we don't use .context(AuthError::TokenExpired) since this is
            // Convex infrastructure error and should be logged as internal error.
            return Err(anyhow::anyhow!("Action callback token expired"));
        }

        Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(issued_s))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        time::{
            Duration,
            SystemTime,
        },
    };

    use cmd_util::env::env_config;
    use common::{
        bootstrap_model::index::database_index::IndexedFields,
        index::IndexKey,
        query::{
            Cursor,
            CursorPosition,
            Order,
            Query,
        },
        query_journal::QueryJournal,
        runtime::Runtime,
        types::{
            MemberId,
            PersistenceVersion,
            TableName,
        },
        value::DeveloperDocumentId,
    };
    use pb::convex_keys::{
        admin_key::Identity as AdminIdentityProto,
        AdminKey as AdminKeyProto,
    };
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;
    use runtime::testing::TestDriver;

    use super::{
        AdminKey,
        KeyBroker,
        ADMIN_KEY_VERSION,
    };
    use crate::{
        AdminIdentity,
        Identity,
    };

    #[test]
    fn test_admin_keys() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let key = kb.issue_admin_key(MemberId(0));
        let admin = kb.check_admin_key(&key.to_string()).unwrap();
        assert!(admin.is_admin());
        assert!(!admin.is_system());
        Ok(())
    }

    #[test]
    fn test_system_keys() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let key = kb.issue_system_key();
        let system = kb.check_admin_key(&key.to_string())?;
        assert!(!system.is_admin());
        assert!(system.is_system());
        Ok(())
    }

    #[test]
    fn test_admin_keys_with_prefix() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let key = kb.issue_admin_key(MemberId(0));
        let prefixed_key = format!("prod:{}", key);
        let admin = kb.check_admin_key(&prefixed_key).unwrap();
        assert!(admin.is_admin());
        assert!(!admin.is_system());
        Ok(())
    }

    fn old_issue_key(kb: &KeyBroker, member_id: Option<MemberId>) -> String {
        let now = SystemTime::now();
        let since_epoch = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to compute seconds since epoch?");

        let identity = match member_id {
            Some(member_id) => AdminIdentityProto::MemberId(member_id.0),
            None => AdminIdentityProto::System(()),
        };
        let proto = AdminKeyProto {
            instance_name: Some(kb.instance_name.clone()),
            issued_s: since_epoch.as_secs(),
            identity: Some(identity),
        };
        kb.encryptor.encode_proto(ADMIN_KEY_VERSION, proto)
    }

    #[test]
    fn test_old_admin_keys() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let key = AdminKey::new(old_issue_key(&kb, Some(MemberId(0))));
        kb.check_admin_key(&key.to_string()).unwrap();
        Ok(())
    }

    #[test]
    fn test_store_file_authorization() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let td = TestDriver::new();
        let now = td.rt().unix_timestamp();
        let key = kb.issue_store_file_authorization(&td.rt(), now)?;
        kb.check_store_file_authorization(&td.rt(), &key.to_string(), Duration::from_secs(60))?;
        Ok(())
    }

    #[test]
    fn test_cant_issue_backwards_timestamps() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let td = TestDriver::new();
        let hour_ago = td.rt().unix_timestamp() - Duration::from_secs(3600);
        kb.issue_store_file_authorization(&td.rt(), hour_ago)
            .unwrap_err();
        Ok(())
    }

    #[test]
    fn test_cursors() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let cursor = Cursor {
            position: CursorPosition::End,
            query_fingerprint: vec![],
        };
        let encrypted = kb.encrypt_cursor(&cursor, PersistenceVersion::default());
        let echoed = kb.decrypt_cursor(encrypted, PersistenceVersion::default())?;
        assert_eq!(cursor, echoed);

        // Add this back if there's a PersistenceVersion that changes cursors
        // let encrypted_old_version = kb.encrypt_cursor(&cursor,
        // PersistenceVersion::V5); let result = kb
        //     .decrypt_cursor(encrypted_old_version, PersistenceVersion::V5)
        //     .unwrap_err();
        // assert!(result.is::<InvalidCursor>());
        Ok(())
    }

    #[test]
    fn test_query_journal_size() -> anyhow::Result<()> {
        // Query journals are synced to the client along with every query
        // result. This test ensures they stay reasonably small.

        // Feel free to bump this values by a little bit, but rethink changes
        // that would increase them by a lot.

        let kb = KeyBroker::dev();

        // Empty journal with no data. Serializes as None/null.
        let empty_journal = QueryJournal::new();
        let serialized_empty_journal =
            kb.encrypt_query_journal(&empty_journal, PersistenceVersion::default());
        assert_eq!(serialized_empty_journal, None);

        // Realistic journal with the end cursor from a paginated query.
        let query = Query::full_table_scan(TableName::from_str("documents")?, Order::Asc);
        let mut journal_with_cursor = QueryJournal::new();
        journal_with_cursor.end_cursor = Some(Cursor {
            position: CursorPosition::After(
                IndexKey::new(vec![100.into()], DeveloperDocumentId::min()).into_bytes(),
            ),
            query_fingerprint: query.fingerprint(&IndexedFields::creation_time())?,
        });
        let serialized_journal_with_cursor =
            kb.encrypt_query_journal(&journal_with_cursor, PersistenceVersion::default());
        assert_eq!(serialized_journal_with_cursor.unwrap().len(), 252);
        Ok(())
    }

    #[test]
    fn test_action_token() -> anyhow::Result<()> {
        let kb = KeyBroker::dev();
        let before_issue = SystemTime::now();
        let token = kb.issue_action_token();
        let after_issue = SystemTime::now();

        // Should be valid if checked with validity of 1 minute.
        let issue_time = kb.check_action_token(&token, Duration::from_secs(60))?;
        // Note we round down the issue time to nearest second.
        assert!(issue_time > before_issue - Duration::from_secs(1));
        assert!(issue_time < after_issue);

        // Should be invalid if checked with validity of 0.
        let err = kb
            .check_action_token(&token, Duration::from_secs(0))
            .unwrap_err();
        assert!(format!("{err}").contains("Action callback token expired"));

        // Try with completely invalid token.
        let err = kb
            .check_action_token(&"invalid-token".to_owned(), Duration::from_secs(60))
            .unwrap_err();
        assert!(format!("{err}").contains("Couldn't decode ActionCallbackTokenProto"));

        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64 * env_config("CONVEX_PROPTEST_MULTIPLIER", 1), failure_persistence: None, .. ProptestConfig::default() })]

        #[test]
        fn test_cursor_roundtrips(cursor in any::<Cursor>()) {
            let kb = KeyBroker::dev();
            let encrypted = kb.encrypt_cursor(&cursor, PersistenceVersion::default());
            let decrypted = kb.decrypt_cursor(encrypted, PersistenceVersion::default()).unwrap();
            assert_eq!(cursor, decrypted);
        }

        #[test]
        fn test_query_journal_roundtrips(journal in any::<QueryJournal>()) {
            let kb = KeyBroker::dev();
            let encrypted = kb.encrypt_query_journal(&journal, PersistenceVersion::default());
            let decrypted = kb.decrypt_query_journal(
                encrypted,
                PersistenceVersion::default(),
            ).unwrap();
            assert_eq!(journal, decrypted);
        }

        #[test]
        fn test_identity_proto_roundtrips(identity in any::<Identity>()) {
            let proto: pb::convex_identity::Identity = identity.clone().into();
            let roundtripped = Identity::from_proto_unchecked(proto).unwrap();
            assert_eq!(identity, roundtripped);
        }

        #[test]
        fn test_admin_identity_proto_roundtrips(admin_identity in any::<AdminIdentity>()) {
            let proto: pb::convex_identity::AdminIdentity = admin_identity
                .clone()
                .into();
            let roundtripped = AdminIdentity::from_proto_unchecked(proto).unwrap();
            assert_eq!(admin_identity, roundtripped);
        }
    }
}
