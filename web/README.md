# Miyu WebUI assets

These static assets are embedded into the Miyu Rust binary at build time. Run the local WebUI with:

```sh
cargo run -- web
```

The server listens on all IPv4 interfaces and prints each available access URL. Open a root URL directly. Password authentication is optional:

```sh
cargo run -- web -p secret
cargo run -- web -p
cargo run -- web --password-file /path/to/password.txt
```

With a password configured, the WebUI prompts for it and establishes a same-origin session after login.
