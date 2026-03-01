# Writing enclave YAML

An enclaves directory has one subdirectory per enclave. Each enclave subdirectory contains a `config.yml` and one subdirectory per partition.

```text
enclaves/
  product-a/dev/
    config.yml          ← enclave declaration
    api/
      config.yml        ← partition declaration (terraform backend)
      main.tf           ← your terraform code
    db/
      config.yml        ← partition declaration (terraform backend)
      main.tf           ← your terraform code
```

## Enclave `config.yml`

```yaml
id: product-a-dev
name: Product A Development
cloud: local          # local | gcp | azure | aws  — optional; omit to use the API's default cloud
region: local-1
identity: product-a-dev-identity

network:
  vpc_cidr: "10.0.0.0/16"
  subnets: ["10.0.1.0/24"]

dns:
  zone: product-a.dev.local

# What this enclave exposes to others
exports:
  - name: api-http
    target_partition: api   # which partition backs this export
    type: http              # http | tcp | queue
    to: any_enclave         # public | any_enclave | vpn | {enclave: <id>}
    auth: token             # none | token | oauth | mtls | native

# What this enclave pulls in from others (cross-enclave)
imports: []
```

**Auth/type compatibility matrix:**

| type | none | token | oauth | mtls | native |
|---|:---:|:---:|:---:|:---:|:---:|
| http | yes | yes | yes | yes | — |
| tcp | yes | — | — | yes | yes |
| queue | yes | yes | — | — | yes |

## Partition `config.yml`

Every partition is backed by Terraform or OpenTofu. Place `.tf` files alongside `config.yml` in the partition directory and declare the variables you need via `inputs:`:

```yaml
id: db
name: Database
produces: tcp
backend: terraform    # terraform | opentofu

inputs:
  project_id: "{{ nclav_project_id }}"   # opt in to context tokens you need
  region:     "{{ nclav_region }}"
  db_host:    "{{ database.hostname }}"  # cross-partition import value

declared_outputs:
  - hostname
  - port
```

When nclav reconciles this partition it:

1. Creates a workspace at `~/.nclav/workspaces/{enclave_id}/{partition_id}/`
2. Symlinks all `.tf` files from the partition directory into the workspace
3. Writes `nclav_backend.tf` (configures the Terraform HTTP state backend — no separate backend setup required)
4. Writes `nclav_context.auto.tfvars` with a preamble containing `nclav_enclave` and `nclav_partition` (always injected), followed by the keys declared in `inputs:` after resolving all template tokens
5. Runs `terraform init` then `terraform apply -auto-approve`
6. Reads the declared outputs and stores them for downstream partitions to consume
7. Records the full combined log as an `IacRun` record (viewable with `nclav iac logs`)

### `inputs:` template tokens

| Token | Value |
|---|---|
| `{{ nclav_enclave_id }}` | Enclave ID |
| `{{ nclav_partition_id }}` | Partition ID |
| `{{ nclav_project_id }}` | Cloud project ID (GCP: enclave's GCP project; local: `""`) |
| `{{ nclav_region }}` | Cloud region (GCP: configured region; local: `""`) |
| `{{ alias.key }}` | Output of a declared cross-partition import |

Only the keys listed in `inputs:` are written to `nclav_context.auto.tfvars`. Your `.tf` files must declare matching `variable` blocks for whatever keys you use.

## Referencing an external module

Add `terraform.source` instead of writing `.tf` files. nclav generates the entire workspace; the partition directory must contain no `.tf` files:

```yaml
backend: terraform
terraform:
  source: "git::https://github.com/myorg/platform-modules.git//postgres?ref=v1.2.0"

inputs:
  project_id: "{{ nclav_project_id }}"
  region:     "{{ nclav_region }}"

declared_outputs:
  - hostname
  - port
```

## Overriding the binary

Pin a version or use a wrapper:

```yaml
terraform:
  tool: /usr/local/bin/terraform-1.6
```

## Terraform state

State is stored inside nclav via the HTTP backend — no S3 bucket, GCS bucket, or Terraform Cloud account required.

## Teardown

`nclav destroy` or removing the enclave from YAML then re-applying runs `terraform destroy -auto-approve` before removing the enclave from state.
