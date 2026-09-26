#!/usr/bin/env python3
"""A development identity provider for Rusty's OIDC sign-in.

Speaks just enough OpenID Connect for the authorization-code flow with
PKCE: discovery, an authorize page that lets you pick who you are, a token
endpoint that checks the PKCE verifier and answers an HS256-signed ID
token, and userinfo. Standard library only. Never for production.

    python3 scripts/oidc-idp-demo.py --port 8300

Then in Rusty: Config → Sign-in → issuer http://127.0.0.1:8300, client id
rusty-studio, client secret dev-secret (whatever you pass as --secret),
and allow 127.0.0.1 on the egress ceiling.
"""
import argparse
import base64
import hashlib
import hmac
import html
import json
import secrets
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

PEOPLE = [
    {"sub": "u-priya-7f3a", "name": "Priya Natarajan", "email": "priya@example.com"},
    {"sub": "u-omar-91c2", "name": "Omar Haddad", "email": "omar@example.com"},
]


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


class Idp:
    def __init__(self, issuer: str, client_id: str, secret: str):
        self.issuer = issuer
        self.client_id = client_id
        self.secret = secret
        self.codes = {}  # code -> {sub, nonce, challenge, redirect_uri, issued}

    def discovery(self):
        return {
            "issuer": self.issuer,
            "authorization_endpoint": f"{self.issuer}/authorize",
            "token_endpoint": f"{self.issuer}/token",
            "userinfo_endpoint": f"{self.issuer}/userinfo",
            "jwks_uri": f"{self.issuer}/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["HS256"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["openid", "profile", "email"],
        }

    def id_token(self, person, nonce):
        now = int(time.time())
        header = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
        payload = b64url(json.dumps({
            "iss": self.issuer, "sub": person["sub"], "aud": self.client_id,
            "exp": now + 600, "iat": now, "nonce": nonce,
            "name": person["name"], "email": person["email"], "preferred_username": person["email"].split("@")[0],
        }).encode())
        signing = f"{header}.{payload}".encode()
        signature = b64url(hmac.new(self.secret.encode(), signing, hashlib.sha256).digest())
        return f"{header}.{payload}.{signature}"


IDP: Idp = None  # set in main


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # quieter
        print("[idp]", fmt % args)

    def _json(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _html(self, status, body):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(url.query))
        if url.path == "/.well-known/openid-configuration":
            return self._json(200, IDP.discovery())
        if url.path == "/jwks":
            return self._json(200, {"keys": []})
        if url.path == "/authorize":
            if q.get("client_id") != IDP.client_id or q.get("response_type") != "code":
                return self._html(400, "<p>unknown client or response type</p>")
            if q.get("code_challenge_method") != "S256" or not q.get("code_challenge"):
                return self._html(400, "<p>PKCE S256 is required here</p>")
            hidden = "".join(f'<input type="hidden" name="{html.escape(k)}" value="{html.escape(v)}">' for k, v in q.items())
            people = "".join(
                f'<button name="sub" value="{html.escape(p["sub"])}" style="display:block;margin:8px 0;padding:10px 16px;font-size:15px">Continue as {html.escape(p["name"])} &lt;{html.escape(p["email"])}&gt;</button>'
                for p in PEOPLE
            )
            return self._html(200, f"""<!doctype html><html><head><meta charset="utf-8"><title>Dev IdP</title></head>
<body style="font-family: system-ui, sans-serif; padding: 40px; max-width: 560px">
<h1 style="font-size:20px">Example Identity — development provider</h1>
<p>Rusty (client <code>{html.escape(IDP.client_id)}</code>) asks you to sign in. Pick who you are:</p>
<form method="post" action="/authorize">{hidden}{people}</form>
<p style="color:#777;font-size:12px">Nothing here is real. This provider signs ID tokens with a shared dev secret.</p>
</body></html>""")
        if url.path == "/userinfo":
            auth = self.headers.get("Authorization", "")
            token = auth.split(" ", 1)[1] if " " in auth else ""
            sub = token.replace("at-", "", 1)
            person = next((p for p in PEOPLE if p["sub"] == sub), None)
            if not person:
                return self._json(401, {"error": "invalid_token"})
            return self._json(200, {"sub": person["sub"], "name": person["name"], "email": person["email"]})
        self._html(404, "<p>not here</p>")

    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        length = int(self.headers.get("Content-Length", "0"))
        form = dict(urllib.parse.parse_qsl(self.rfile.read(length).decode()))
        if url.path == "/authorize":
            person = next((p for p in PEOPLE if p["sub"] == form.get("sub")), None)
            if not person:
                return self._html(400, "<p>unknown person</p>")
            code = "c-" + secrets.token_urlsafe(24)
            IDP.codes[code] = {"sub": person["sub"], "nonce": form.get("nonce", ""), "challenge": form.get("code_challenge", ""), "redirect_uri": form.get("redirect_uri", ""), "issued": time.time()}
            back = form["redirect_uri"] + ("&" if "?" in form["redirect_uri"] else "?") + urllib.parse.urlencode({"code": code, "state": form.get("state", "")})
            self.send_response(303)
            self.send_header("Location", back)
            self.end_headers()
            return
        if url.path == "/token":
            if form.get("grant_type") != "authorization_code":
                return self._json(400, {"error": "unsupported_grant_type"})
            if form.get("client_id") != IDP.client_id or form.get("client_secret") != IDP.secret:
                return self._json(401, {"error": "invalid_client", "error_description": "client id or secret is wrong"})
            issued = IDP.codes.pop(form.get("code", ""), None)
            if not issued or time.time() - issued["issued"] > 300:
                return self._json(400, {"error": "invalid_grant", "error_description": "the code is unknown, used, or expired"})
            if issued["redirect_uri"] != form.get("redirect_uri"):
                return self._json(400, {"error": "invalid_grant", "error_description": "redirect_uri does not match"})
            verifier = form.get("code_verifier", "")
            if b64url(hashlib.sha256(verifier.encode()).digest()) != issued["challenge"]:
                return self._json(400, {"error": "invalid_grant", "error_description": "PKCE verifier does not match the challenge"})
            person = next(p for p in PEOPLE if p["sub"] == issued["sub"])
            return self._json(200, {
                "access_token": "at-" + person["sub"], "token_type": "Bearer", "expires_in": 600,
                "id_token": IDP.id_token(person, issued["nonce"]), "scope": "openid profile email",
            })
        self._json(404, {"error": "not_found"})


def main():
    global IDP
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8300)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--client-id", default="rusty-studio")
    ap.add_argument("--secret", default="dev-secret")
    args = ap.parse_args()
    issuer = f"http://{args.host}:{args.port}"
    IDP = Idp(issuer, args.client_id, args.secret)
    print(f"[idp] issuer {issuer} · client {args.client_id} · discovery {issuer}/.well-known/openid-configuration")
    HTTPServer((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
