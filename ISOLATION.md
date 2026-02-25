# nclav Service Catalog & Topology

This document defines the validated service catalog for nclav's isolated enclave topology and provides guidance on when hub-spoke will be required.

## Design Principle

nclav's default topology guarantees network-level isolation between enclaves. Cross-enclave communication happens only through explicit, auditable bridges using cloud-native private connectivity mechanisms (GCP Private Service Connect; Azure Private Link and AWS PrivateLink when those drivers land).

The service catalog enumerates the services where nclav can guarantee this isolation model holds end-to-end — services with native private connectivity support that work without a shared routing plane.

If your workload requires services outside the catalog, you will need hub-spoke topology, a planned nclav mode that provides a shared routing plane with firewall-enforced isolation. This is not a failure mode — it is a different tradeoff for different requirements.

---

## Service Catalog

### Compute (HTTP Targets)

Services that serve HTTP traffic. These bind to `type: http` exports.

| Service | GCP | Azure (future) | AWS (future) |
|---------|-----|----------------|--------------|
| Serverless Containers | Cloud Run via PSC | Container Apps via Private Link | ECS/Fargate via PrivateLink |
| Managed Kubernetes | GKE via PSC | AKS via Private Link | EKS via PrivateLink |
| VM Fleet | MIG + Internal HTTP(S) LB + PSC | VMSS + Std LB + Private Link Service | ASG + ALB + VPC Endpoint Service |

**VM Fleet note:** The load balancer is structural — multiple instances behind it, not overhead. nclav provisions the instance group, load balancer, and private connectivity wiring as a unit. Deployment choreography (rolling, blue/green) is delegated to existing tooling.

### Compute (TCP Targets)

Services exposed as raw TCP endpoints. These bind to `type: tcp` exports.

| Service | GCP | Azure (future) | AWS (future) |
|---------|-----|----------------|--------------|
| VM Fleet (L4) | MIG + Internal TCP/UDP LB + PSC | VMSS + Std LB + Private Link Service | ASG + NLB + VPC Endpoint Service |

L4 exports pass TCP traffic without termination. Suitable for protocols that need raw TCP (database wire protocols, custom binary protocols) when fronting a fleet of instances.

### Data

Managed database and cache services. These bind to `type: tcp` exports. All services listed have native private connectivity — no intermediate load balancer required.

| Service | GCP | Azure (future) | AWS (future) |
|---------|-----|----------------|--------------|
| Managed Postgres | Cloud SQL Postgres (PSC), AlloyDB (PSC) | Azure DB for PostgreSQL (Private Link) | RDS PostgreSQL (PrivateLink) |
| Managed MySQL | Cloud SQL MySQL (PSC) | Azure DB for MySQL (Private Link) | RDS MySQL (PrivateLink) |
| Managed SQL Server | Cloud SQL SQL Server (PSC) | Azure SQL Database (Private Link) | RDS SQL Server (PrivateLink) |
| Distributed DB | Cloud Spanner (PSC) | Cosmos DB (Private Link) | DynamoDB (Gateway Endpoint) |
| Managed Redis/Valkey | Memorystore (PSC) | Azure Cache for Redis (Private Link) | ElastiCache (PrivateLink) |

**DynamoDB note:** Uses a Gateway Endpoint (route table entry) rather than an interface endpoint. Functionally equivalent — traffic stays on the AWS backbone — but mechanically different. The driver handles this transparently.

### Object Storage

| Service | GCP | Azure (future) | AWS (future) |
|---------|-----|----------------|--------------|
| Blob/Object Storage | Cloud Storage (PSC via Google APIs bundle) | Azure Storage (Private Link) | S3 (Gateway Endpoint or PrivateLink) |

GCP Cloud Storage is accessed via the Google APIs PSC bundle rather than per-bucket attachments. S3 supports both a free Gateway Endpoint (route-table based, sufficient for most cases) and a paid PrivateLink interface endpoint.

### Messaging

Queue and event services. These bind to `type: queue` exports. Connectivity is at the API level — the consumer connects to the service's API endpoint privately, not to a per-resource attachment.

| Service | GCP | Azure (future) | AWS (future) |
|---------|-----|----------------|--------------|
| Queue | Pub/Sub (PSC via Google APIs) | Service Bus (Private Link, premium tier) | SQS (PrivateLink) |
| Event Streaming | Pub/Sub (PSC via Google APIs) | Event Hubs (Private Link, premium tier) | Kinesis (PrivateLink) |
| Event Routing | Eventarc (via Pub/Sub) | Event Grid (Private Link) | EventBridge (PrivateLink) |

**Azure tier note:** Service Bus and Event Hubs require the premium tier for Private Link. This is a cost consideration worth surfacing at planning time.

---

## What the Catalog Means for Partitions

Each catalog entry maps to a partition backend that provisions both the resource and its membrane crossing. A Cloud SQL partition, for example, does not just create a database — it creates a database with a PSC service attachment that nclav reads to wire cross-enclave TCP imports.

```yaml
# enclaves/product-a/dev/db/config.yml
id: db
name: Database
produces: tcp
backend: terraform

inputs:
  project_id: "{{ nclav_project_id }}"
  region: "{{ nclav_region }}"

declared_outputs:
  - hostname
  - port
  - psc_service_attachment   # nclav reads this to wire the PSC endpoint in importers
```

```hcl
# enclaves/product-a/dev/db/main.tf
resource "google_sql_database_instance" "db" {
  database_version = "POSTGRES_15"

  settings {
    ip_configuration {
      ipv4_enabled = false
      psc_config { psc_enabled = true }
    }
  }
}

output "psc_service_attachment" {
  value = google_sql_database_instance.db.psc_service_attachment_link
}
```

The partition Terraform creates the resource with private connectivity enabled and exposes the service attachment URI as an output. nclav reads that output and creates the consumer-side PSC endpoint (and DNS record) when another enclave imports the export. See [PARTITIONS.md](PARTITIONS.md) for the full partition authoring reference.

For catalog services, no intermediate load balancer is needed (except VM fleets, where the LB is structural). The managed service exposes a native service attachment directly.

---

## Topology Decision Guide

### Isolated Topology (Default)

nclav's current mode. Use it when your workloads use catalog services — the majority of enterprise applications:

- HTTP APIs on managed containers or VM fleets
- Managed relational databases (Postgres, MySQL, SQL Server)
- Managed caches (Redis, Valkey)
- Object storage
- Managed queues and event buses
- Any combination of the above

What isolated topology provides:
- True network-level isolation (no shared routing plane between enclaves)
- No CIDR coordination required between enclave owners
- Every cross-enclave connection is an explicit, auditable bridge
- Each connection is independently revocable
- Consumer enclaves see a local IP, never the provider's internal addressing

### Hub-Spoke Topology (Planned)

A future nclav topology providing a shared routing plane with firewall-enforced isolation. You will need it when your workloads require services or patterns outside the catalog:

**Wire protocol services that embed addressing:**
- Apache Kafka — brokers advertise internal addresses in metadata responses
- Apache Cassandra — gossip protocol exposes node IPs via `system.peers`
- Elasticsearch — transport protocol for inter-node communication
- ZooKeeper — ensemble member discovery

These protocols assume network-level adjacency between producers and consumers. They break when traffic passes through address translation layers (NAT, private connectivity endpoints). They need a shared routing plane where advertised addresses are directly reachable.

**Stateful clustering across enclaves:**
- Database replication where replicas span multiple enclaves
- Distributed caches with cross-enclave sharding
- Any protocol that requires bidirectional connection initiation

**Legacy or custom services:**
- Services without any private connectivity mechanism
- Custom TCP services with embedded IP semantics
- Protocols requiring multicast or broadcast

**High-density cross-enclave communication:**
- N×M topologies where private endpoint proliferation becomes operationally expensive
- Latency-sensitive paths where the private connectivity hop adds unacceptable overhead

### The Tradeoff

| | Isolated (current) | Hub-Spoke (planned) |
|---|---|---|
| Isolation mechanism | Network topology (no route exists) | Firewall rules (route exists, access controlled) |
| CIDR coordination | None required | Centrally managed, non-overlapping |
| Cross-enclave wiring | Per-connection (one PSC/PrivateLink endpoint per service) | Implicit (any routable address reachable, filtered by firewall) |
| Wire protocol support | Catalog services only | Any protocol |
| Audit granularity | Each connection is a discrete, revocable resource | Firewall rule sets, harder to audit per-service |
| Blast radius of misconfiguration | Limited to one connection | Potential lateral movement across routing plane |
| Central dependency | None | Hub network availability |

### The Wrap Pattern

Before reaching for hub-spoke, consider whether the non-catalog service can be wrapped behind a catalog service:

- Instead of exporting raw Kafka brokers across enclaves, put a consumer-side HTTP API in front (in the same enclave as Kafka) and export that. The API is a catalog service.
- Instead of letting Cassandra gossip across enclaves, keep the cluster inside one enclave and export a data access API.
- Instead of cross-enclave database replication, use the managed database's built-in replication (which works through the cloud's internal fabric, not your network).

This is not always possible, but when it is, it is architecturally stronger. It forces service boundaries at the enclave membrane rather than leaking wire protocols. The enclave boundary becomes an API boundary — which is usually the right decomposition.

### Decision Flowchart

```
Is every service in the catalog?
  ├── Yes → isolated topology (default)
  └── No  → Can you wrap non-catalog services behind a catalog service (HTTP API)?
              ├── Yes → isolated topology (wrap pattern)
              └── No  → Do you need raw network adjacency?
                          ├── Yes → hub-spoke topology (planned)
                          └── No  → Re-evaluate architecture
```

---

## Related Documents

- [PRD.md](PRD.md) — Core concepts, imports/exports, enclave and partition primitives
- [PARTITIONS.md](PARTITIONS.md) — Partition backends, Terraform/OpenTofu, inputs/outputs, labeling
- [GCP.md](GCP.md) — GCP driver reference, PSC wiring, Cloud Run, Cloud SQL, IAM
- [BOOTSTRAP.md](BOOTSTRAP.md) — Platform setup, authentication, bootstrap options
