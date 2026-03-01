# HTTP API reference

Start the server with `nclav serve`, then use the token from `~/.nclav/token`. All endpoints require `Authorization: Bearer <token>`.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Always 200 OK |
| `GET` | `/ready` | 200 if store is reachable |
| `POST` | `/reconcile` | Apply changes |
| `POST` | `/reconcile/dry-run` | Diff only |
| `GET` | `/enclaves` | List all enclave states |
| `GET` | `/enclaves/{id}` | Single enclave state |
| `DELETE` | `/enclaves/{id}` | Destroy an enclave and all its infrastructure |
| `GET` | `/enclaves/{id}/graph` | Import/export graph for one enclave |
| `GET` | `/graph` | System-wide dependency graph |
| `GET` | `/events` | Audit log (`?enclave_id=&limit=`) |
| `GET` | `/status` | Summary: enclave count, default cloud, active drivers |
| `DELETE` | `/enclaves/{id}/partitions/{part}` | Destroy a single partition and its infrastructure |
| `GET` | `/enclaves/{id}/partitions/{part}/iac/runs` | List IaC runs for a partition |
| `GET` | `/enclaves/{id}/partitions/{part}/iac/runs/latest` | Most recent IaC run |
| `GET` | `/enclaves/{id}/partitions/{part}/iac/runs/{run-id}` | Specific IaC run |
| `GET` | `/terraform/state/{enc}/{part}` | TF HTTP backend: get state |
| `POST` | `/terraform/state/{enc}/{part}` | TF HTTP backend: save state |
| `DELETE` | `/terraform/state/{enc}/{part}` | TF HTTP backend: delete state |
| `POST` | `/terraform/state/{enc}/{part}/lock` | TF HTTP backend: acquire lock |
| `DELETE` | `/terraform/state/{enc}/{part}/lock` | TF HTTP backend: release lock. Send no body to force-unlock (clears any lock regardless of ID) |

## Examples

```bash
TOKEN=$(cat ~/.nclav/token)

# Apply via HTTP
curl -X POST http://localhost:8080/reconcile \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"enclaves_dir": "./enclaves"}'

# Destroy an enclave via HTTP
curl -X DELETE http://localhost:8080/enclaves/product-a-dev \
  -H "Authorization: Bearer $TOKEN"

# Audit log
curl -H "Authorization: Bearer $TOKEN" 'http://localhost:8080/events?limit=20'
```
