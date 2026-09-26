# Plugin generators

The three desk plugins under `catalog/plugins/` (`it-service-desk`, `sales-desk`,
`hr-desk`) are generated from these scripts so a connector operation or a skill's
method can be changed in one place and regenerated:

    python3 tools/plugin-gen/gen_it_desk.py
    python3 tools/plugin-gen/gen_sales_hr.py

`plugin_gen.py` holds the manifest and skill helpers (`manifest`, `op`, `spec`,
`skill`, `write_plugin`). Manifests carry no hash; the server seals them when
the plugin is installed (`POST /plugins/install {library}`), and refuses one
that breaks a rule (a GET must be `read_only`, a DELETE `irreversible`, a grant
flow needs scopes, every path placeholder must be a declared parameter). The
server reads the library from `catalog/plugins/*` at boot, so restart after
regenerating.
