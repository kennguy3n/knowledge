package connector

import (
	"crypto/rand"
	"encoding/hex"
	"net/url"
)

// OAuthProvider describes a connector kind's OAuth2 authorization
// endpoint and the scopes requested.
type OAuthProvider struct {
	// AuthorizeURL is the provider's authorization endpoint.
	AuthorizeURL string
	// Scopes are the OAuth2 scopes requested.
	Scopes []string
}

// defaultProviders maps connector kinds to their OAuth2 authorization
// endpoints. These are the public provider endpoints; client ids are
// supplied per connector instance.
//
// Keys are the on-the-wire ConnectorKindTag values, which serialize as
// snake_case (`ffi::ConnectorKindTag` is `#[serde(rename_all =
// "snake_case")]`) — the exact strings the admin SPA sends as `kind`
// and the substrate stores. authorizeURL looks up `reg.Kind` verbatim,
// so a PascalCase key here would never match and every OAuth start
// would 400.
var defaultProviders = map[string]OAuthProvider{
	"google_drive": {
		AuthorizeURL: "https://accounts.google.com/o/oauth2/v2/auth",
		Scopes:       []string{"https://www.googleapis.com/auth/drive.readonly"},
	},
	"one_drive": {
		AuthorizeURL: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
		Scopes:       []string{"Files.Read.All", "offline_access"},
	},
	"notion": {
		AuthorizeURL: "https://api.notion.com/v1/oauth/authorize",
		Scopes:       nil,
	},
	"slack": {
		AuthorizeURL: "https://slack.com/oauth/v2/authorize",
		Scopes:       []string{"channels:history", "channels:read"},
	},
	"git_hub": {
		AuthorizeURL: "https://github.com/login/oauth/authorize",
		Scopes:       []string{"repo", "read:org"},
	},
	"jira": {
		AuthorizeURL: "https://auth.atlassian.com/authorize",
		Scopes:       []string{"read:jira-work", "offline_access"},
	},
	"confluence": {
		AuthorizeURL: "https://auth.atlassian.com/authorize",
		Scopes:       []string{"read:confluence-content.all", "offline_access"},
	},
}

// authorizeURL builds the provider authorization URL for a connector
// kind, embedding the client id, redirect URI and CSRF state. It
// returns false if the kind has no known OAuth provider.
func authorizeURL(kind, clientID, redirectURI, state string) (string, bool) {
	p, ok := defaultProviders[kind]
	if !ok {
		return "", false
	}
	q := url.Values{}
	q.Set("client_id", clientID)
	q.Set("redirect_uri", redirectURI)
	q.Set("response_type", "code")
	q.Set("state", state)
	if len(p.Scopes) > 0 {
		scope := p.Scopes[0]
		for _, s := range p.Scopes[1:] {
			scope += " " + s
		}
		q.Set("scope", scope)
	}
	return p.AuthorizeURL + "?" + q.Encode(), true
}

// newState mints a cryptographically random CSRF state token.
func newState() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}
