# Topcamp → Topcoat + DBOS + PostgreSQL + RustFS

## Mission

Re-architect `basecamp/once-campfire-rust` into a modern Rust application built around:

* **Topcoat** — primary HTTP/application framework
* **PostgreSQL** — relational persistence and full-text search
* **DBOS Transact for Rust** — durable workflows, jobs, retries, scheduling, and integrations
* **RustFS** — S3-compatible object/blob storage
* **Tokio + custom Cable implementation** — realtime WebSocket / Action Cable protocol
* Existing Topcamp domain behavior — compatibility specification

This is **not** a mechanical port.

The existing Topcamp implementation is the behavioral reference. Its current crate boundaries, HTTP abstractions, controllers, database abstractions, job system, and application plumbing are **not requirements to preserve**.

The goal is to produce a clean, idiomatic, Topcoat-native Rust implementation that preserves Topcamp's externally observable behavior while substantially simplifying its architecture.

---

# 1. Core Principle

Use this rule throughout the migration:

> **Preserve behavior, not implementation.**

If existing Topcamp code has an abstraction that Topcoat, DBOS, PostgreSQL, RustFS, or Tokio already provides more cleanly, remove the old abstraction.

Do not create compatibility wrappers merely to make old code survive.

Do not translate Ruby/Rails/Topcamp architectural concepts one-for-one into Rust.

Do not preserve a crate merely because it currently exists.

Do not preserve a controller/service/repository abstraction merely because the old application has one.

The resulting architecture should look like a Rust application designed around these technologies from the beginning.

---

# 2. Mandatory First Step: Repository + Dependency Audit

Before changing code:

1. Inspect the entire repository.
2. Understand every crate.
3. Trace the main request lifecycle.
4. Trace authentication/session handling.
5. Trace message creation.
6. Trace Cable/WebSocket handling.
7. Trace jobs.
8. Trace database writes and events.
9. Trace search.
10. Trace storage/uploads.
11. Trace rich text.
12. Trace external integrations.
13. Identify existing tests and behavioral guarantees.

Then inspect the **actual current source/API** of:

* Topcoat
* DBOS Transact Rust
* RustFS / S3-compatible API
* Tokio WebSocket ecosystem

Do not rely on memory of APIs.

Do not invent APIs.

Do not copy an API from another language implementation of DBOS.

Pin versions based on the actual repositories/package manifests available during implementation.

If a feature differs from documentation or examples, use the actual pinned source as authoritative.

Create:

```text
MIGRATION_NOTES.md
```

containing:

* current architecture
* target architecture
* dependency versions
* crate migration map
* deleted abstractions
* important behavioral compatibility decisions
* known limitations

---

# 3. Target Architecture

The target architecture is:

```text
                         ┌─────────────────────┐
                         │       Topcoat       │
                         │                     │
                         │ Router              │
                         │ Requests            │
                         │ Responses           │
                         │ Pages / HTML        │
                         │ Layers              │
                         │ Forms               │
                         │ Cookies             │
                         │ Query/path params   │
                         └──────────┬──────────┘
                                    │
                                    ▼
                         ┌─────────────────────┐
                         │   Application       │
                         │                     │
                         │ Use cases           │
                         │ Auth/context        │
                         │ Authorization       │
                         │ Domain orchestration│
                         └──────────┬──────────┘
                                    │
               ┌────────────────────┼────────────────────┐
               │                    │                    │
               ▼                    ▼                    ▼
       ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
       │ PostgreSQL   │     │    DBOS      │     │   RustFS     │
       │              │     │              │     │              │
       │ domain data  │     │ workflows    │     │ blobs        │
       │ transactions │     │ jobs         │     │ attachments  │
       │ search       │     │ retries      │     │ objects      │
       └──────────────┘     │ integrations │     └──────────────┘
                            └──────────────┘

                         ┌─────────────────────┐
                         │   Cable / Tokio     │
                         │                     │
                         │ WebSocket           │
                         │ Action Cable        │
                         │ subscriptions       │
                         │ heartbeats          │
                         │ broadcasts          │
                         └─────────────────────┘
```

Topcoat is the **application/web boundary**.

PostgreSQL is the **source of truth for relational state**.

DBOS is the **durable asynchronous execution boundary**.

RustFS is the **blob/object boundary**.

Cable is the **realtime boundary**.

The domain remains ordinary Rust code.

---

# 4. Topcoat Must Be Used Fully

Do not treat Topcoat as merely an Axum replacement.

Before introducing a custom abstraction, check whether Topcoat already provides the required primitive.

Audit and use, where appropriate:

* router
* routes
* nested routes
* path parameters
* query parameters
* request
* response
* body handling
* body limits
* URL encoded forms
* methods
* layers
* Tower integration
* connection handling
* error handling
* redirects
* pages
* components
* static assets
* origin handling
* trailing slash behavior
* URL generation
* request-scoped state/context
* middleware/layers

The application should feel like a **Topcoat application**, not an Axum application with the Axum types renamed.

---

# 5. Topcoat Capability Audit

Before implementing the web layer, produce an internal mapping:

| Existing Topcamp behavior | Topcoat capability                      | Action          |
| -------------------------- | --------------------------------------- | --------------- |
| routes                     | Router                                  | replace         |
| path params                | Path params                             | replace         |
| query parsing              | Query params                            | replace         |
| form parsing               | URL encoded/body primitives             | replace         |
| request context            | Request/context                         | simplify        |
| response wrappers          | Response                                | simplify        |
| redirects                  | Response/router primitive               | simplify        |
| middleware                 | Layer/Tower                             | replace         |
| HTTP errors                | Topcoat error/response                  | simplify        |
| static assets              | Topcoat integration                     | replace         |
| HTML rendering             | Topcoat pages/components where suitable | redesign        |
| request authentication     | Topcoat layer/context                   | redesign        |
| cookies                    | Topcoat HTTP primitives                 | redesign        |
| WebSocket endpoint         | Topcoat connection integration          | integrate Cable |
| existing Axum router       | Topcoat Router                          | remove          |

Do not implement the old abstraction first and "optimize later".

The initial implementation should already use the correct Topcoat architecture.

---

# 6. Remove Axum as the Primary Framework

Topcamp currently uses Axum.

Replace Axum as the primary HTTP framework.

Target:

```text
Topcoat Router
        ↓
Topcoat request
        ↓
application/use case
        ↓
Topcoat response/page
```

Use Topcoat's Tower integration where existing Tower ecosystem functionality is genuinely useful.

Do not keep Axum simply because it makes migration easier.

Temporary compatibility code is acceptable during intermediate compilation, but it must be removed before completion.

Final application routing must be Topcoat-native.

---

# 7. Application Boundary

Create a clean application boundary between HTTP and domain behavior.

Conceptually:

```text
web/
    routes
    pages
    request handling
    authentication extraction
    response construction

application/
    use cases
    authorization
    orchestration

domain/
    users
    rooms
    messages
    memberships
    rich text
    business rules

infrastructure/
    postgres
    dbos
    rustfs
    cable
```

Do not blindly reproduce this directory structure if another arrangement is cleaner.

The important rule is:

> HTTP concerns must not leak deeply into domain logic.

Likewise:

> Domain logic must not depend on Topcoat.

---

# 8. Authentication and Authorization

Move HTTP-level authentication concerns into Topcoat-compatible request/layer/context handling.

Authentication should roughly follow:

```text
request
   ↓
Topcoat layer/context
   ↓
session extraction
   ↓
authenticated user
   ↓
application use case
   ↓
authorization
   ↓
domain operation
```

Keep authorization rules in application/domain code where they represent business rules.

Do not embed authorization into individual HTTP handlers if it belongs to the domain.

Avoid a giant global request context containing unrelated state.

Use explicit dependencies.

---

# 9. Sessions and Cookies

Replace framework-specific or Topcamp-specific HTTP plumbing with Topcoat primitives wherever possible.

Implement:

* session extraction
* session validation
* cookie creation
* cookie deletion
* secure cookie configuration
* expiration
* authentication redirects

without creating redundant HTTP wrappers.

Session persistence belongs in PostgreSQL.

---

# 10. HTML / Pages / Views

Audit the current Askama/Topcamp view system.

Do not blindly port every view abstraction.

Use Topcoat's page/component facilities where they provide a cleaner application model.

Preserve:

* generated HTML semantics
* forms
* links
* redirects
* CSRF behavior
* error rendering
* pagination
* message display
* room display
* authentication pages

Where an existing template is already appropriate and Topcoat can integrate it cleanly, retain the template engine rather than rewriting HTML for no benefit.

The objective is not "remove Askama at all costs".

The objective is:

> remove unnecessary view/application plumbing and use Topcoat's native page/application model where advantageous.

---

# 11. PostgreSQL Replaces SQLite

Replace the SQLite-backed `topcamp_db` architecture with PostgreSQL.

Do not attempt a literal SQLite → PostgreSQL syntax conversion.

Design a proper PostgreSQL schema.

Review:

* UUIDs/IDs
* timestamps
* booleans
* nullable fields
* JSON
* enums
* foreign keys
* unique constraints
* indexes
* cascading behavior
* transaction boundaries
* locking
* concurrency

Use an async PostgreSQL driver compatible with the selected architecture.

Prefer explicit SQL and focused repository functions over introducing a large ORM unless the repository demonstrates a compelling reason.

---

# 12. Database Architecture

The database layer should provide focused repositories or equivalent abstractions.

Examples:

```text
UserRepository
RoomRepository
MembershipRepository
MessageRepository
SessionRepository
AttachmentRepository
WebhookRepository
SubscriptionRepository
```

Do not create a generic:

```text
DatabaseService
```

containing hundreds of unrelated methods.

Repositories should expose domain-oriented operations.

For example:

```text
create_message(...)
find_room(...)
list_messages(...)
add_membership(...)
remove_membership(...)
```

rather than leaking SQL into HTTP handlers.

---

# 13. Transactions

Preserve important transaction boundaries.

A message creation operation should remain atomic with the relational state that must be committed together.

Conceptually:

```text
BEGIN

validate state
create message
create related records
update required relational state

COMMIT
```

Do not use DBOS as a replacement for ordinary database transactions.

DBOS provides durable workflow execution.

PostgreSQL provides transactional state.

These are different responsibilities.

---

# 14. Database Events

Topcamp currently has event-driven behavior around database writes.

Redesign this carefully.

The preferred model is:

```text
Postgres transaction
        │
        │ commit
        ▼
 durable event/workflow scheduling
        │
        ▼
       DBOS
```

Do not trigger irreversible external side effects before the transaction has successfully committed.

Where reliable post-commit delivery is required, introduce an appropriate transactional/outbox pattern rather than hoping an in-process callback will always execute.

---

# 15. Search

Remove SQLite FTS5.

Implement PostgreSQL full-text search.

Use PostgreSQL facilities such as:

* `tsvector`
* `tsquery`
* GIN indexes
* ranking functions
* PostgreSQL text search configuration

Preserve existing Topcamp search semantics as closely as practical.

Test:

* Unicode
* punctuation
* whitespace
* malformed queries
* empty queries
* AND semantics
* ranking
* pagination
* snippets/highlighting if currently supported

Search is part of PostgreSQL infrastructure.

DBOS should not become the search engine.

---

# 16. DBOS

Use DBOS Transact Rust for durable asynchronous execution.

DBOS owns:

* durable workflows
* background jobs
* retries
* scheduled work
* long-running workflows
* external integrations
* asynchronous processing
* notification workflows
* webhook workflows
* attachment processing workflows
* other work where crash recovery matters

DBOS does NOT own:

* ordinary CRUD
* PostgreSQL transactions
* HTTP routing
* WebSockets
* Cable
* rich text
* blob storage
* ordinary synchronous domain logic

---

# 17. DBOS API Rule

The Rust DBOS implementation is authoritative.

Before writing DBOS code:

1. inspect the pinned DBOS Rust version
2. inspect its actual API
3. inspect its examples
4. inspect its tests if necessary
5. use only APIs that actually exist

Never fabricate:

* workflow macros
* transaction APIs
* decorators
* runtime methods
* scheduling APIs
* retry APIs

from another DBOS language.

If the Rust implementation lacks a desired feature, design around the limitation rather than inventing an API.

---

# 18. DBOS Workflow Design

Every durable workflow should be:

* deterministic where required
* retry-safe
* idempotent
* explicit about external side effects

Examples:

```text
ProcessAttachment
SendNotification
DeliverWebhook
ProcessIntegration
GenerateDerivedAsset
SendEmail
```

External operations must tolerate retries.

Use idempotency keys or durable operation IDs where necessary.

Do not assume:

```text
workflow runs once
```

Assume:

```text
workflow may be retried
workflow may restart
external operation may have partially succeeded
```

---

# 19. DBOS and Message Creation

Do not turn every request into a DBOS workflow.

For synchronous user-visible operations:

```text
HTTP request
   ↓
Topcoat
   ↓
application use case
   ↓
Postgres transaction
   ↓
commit
   ↓
immediate response
```

Then durable asynchronous work can be scheduled:

```text
commit
   ↓
DBOS
   ├── notifications
   ├── webhooks
   ├── integrations
   ├── indexing
   └── other durable work
```

The user should not wait for unrelated durable work.

---

# 20. RustFS Replaces Application Blob Storage

Use RustFS as the physical object store.

The application should expose a small abstraction such as:

```text
BlobStore
```

with operations appropriate to Topcamp, for example:

```text
put
get
head
delete
presign_get
presign_put
```

Add multipart operations if actually required.

Implement:

```text
RustFsBlobStore
```

using the standard S3-compatible Rust ecosystem.

Do not write a RustFS-specific storage protocol.

RustFS should be treated as an S3-compatible object store.

---

# 21. Blob Metadata

Keep metadata in PostgreSQL.

Store in RustFS:

```text
object bytes
```

Store in PostgreSQL:

```text
attachment ID
owner
room/message relationship
object key
filename
content type
size
checksum if required
processing state
created_at
derived asset references
```

Do not make RustFS the source of truth for application metadata.

---

# 22. Attachment Processing

The preferred architecture:

```text
upload
  ↓
RustFS
  ↓
Postgres attachment record
  ↓
DBOS workflow
  ├── validation
  ├── metadata extraction
  ├── content processing
  ├── thumbnail/derived assets
  └── notifications
```

The original uploaded object must not be destroyed merely because derived processing fails.

Processing should be retryable.

---

# 23. Rich Text

Keep rich text as ordinary Rust/domain functionality.

`topcamp_richtext` should not depend on DBOS.

It should not depend on Topcoat.

It should not depend on PostgreSQL.

It should provide focused operations such as:

```text
parse
render
plain_text
mentions
sanitize
```

Integrate it with the application/domain layer.

If the existing rich-text API can be substantially simplified, simplify it.

---

# 24. Cable

Do NOT replace Topcamp Cable with DBOS.

Do NOT replace the Action Cable protocol with a generic WebSocket abstraction.

Cable is realtime infrastructure.

Preserve:

* WebSocket upgrade
* Action Cable protocol
* protocol negotiation
* connection lifecycle
* authentication
* subscriptions
* unsubscribe
* heartbeats
* broadcasts
* fanout
* presence
* typing/activity behavior
* disconnect handling

Use Tokio for runtime/concurrency.

Integrate Cable with Topcoat's connection/HTTP boundary.

The final HTTP endpoint should conceptually be:

```text
/cable
```

---

# 25. Cable and PostgreSQL

Cable should use the new PostgreSQL/application layer rather than old SQLite-specific models.

Avoid putting database access logic directly inside WebSocket protocol code.

Prefer:

```text
Cable
  ↓
application/domain
  ↓
repositories
  ↓
PostgreSQL
```

---

# 26. Cable and DBOS

DBOS may initiate durable work that eventually results in a Cable broadcast.

However, do not route every realtime event through DBOS.

For immediate user-visible events:

```text
Postgres commit
     ↓
Cable broadcast
```

For durable asynchronous behavior:

```text
Postgres commit
     ↓
DBOS workflow
     ↓
event/result
     ↓
Cable broadcast if required
```

Realtime delivery and durable execution are different concerns.

---

# 27. Integrations

Move external integrations onto DBOS where durability/retry is useful.

Examples:

```text
webhooks
email
push notifications
third-party APIs
external callbacks
integration synchronization
```

An integration should look conceptually like:

```text
application
   ↓
DBOS workflow
   ↓
external service
   ↓
retry / durable result
```

External side effects must be idempotent.

---

# 28. Jobs

Remove the existing Topcamp job system.

Do not simply rename:

```text
topcamp_jobs
```

to:

```text
dbos_jobs
```

Re-evaluate every existing job.

For each job ask:

1. Does it still need to exist?
2. Is it synchronous instead?
3. Should it become a DBOS workflow?
4. Can multiple jobs become one workflow?
5. Does it need retries?
6. Does it need scheduling?
7. Does it require idempotency?
8. Does it depend on an external service?

Delete obsolete jobs.

---

# 29. Topcoat Layers

Use Topcoat/Tower layers for cross-cutting HTTP concerns.

Potential layers include:

```text
request ID
logging
authentication extraction
security headers
compression
CSRF
session handling
metrics
error handling
```

Do not put business logic into middleware.

A middleware should be reusable and focused.

---

# 30. Request Flow

A typical request should look like:

```text
HTTP
 ↓
Topcoat router
 ↓
Topcoat layers
 ↓
authenticated request/context
 ↓
page/handler
 ↓
application use case
 ↓
domain
 ↓
Postgres
 ↓
DBOS / Cable when required
 ↓
Topcoat response
```

Avoid:

```text
HTTP
 ↓
giant controller
 ↓
giant service
 ↓
giant database object
```

---

# 31. Error Handling

Create a coherent error model.

Separate:

```text
domain errors
application errors
database/infrastructure errors
HTTP errors
```

Do not leak SQL errors or internal infrastructure details directly to users.

Map errors at the appropriate boundary.

Topcoat should own the HTTP representation.

---

# 32. State Management

Application state should contain only genuinely shared infrastructure.

Conceptually:

```rust
AppState {
    config,
    database,
    blob_store,
    dbos,
    cable,
    clock,
    ...
}
```

Avoid putting arbitrary request/user/domain state into global application state.

Request-specific information belongs to the request/context.

---

# 33. Configuration

Provide clear configuration for:

```text
HTTP
DATABASE
DBOS
RUSTFS
AUTH
CABLE
EMAIL
WEB_PUSH
INTEGRATIONS
```

At minimum support:

```text
DATABASE_URL

RUSTFS_ENDPOINT
RUSTFS_ACCESS_KEY
RUSTFS_SECRET_KEY
RUSTFS_BUCKET
RUSTFS_REGION
```

Document all required configuration.

Create/update:

```text
.env.example
```

Never commit secrets.

---

# 34. Local Development

Provide local infrastructure for:

```text
PostgreSQL
RustFS
```

Prefer Docker Compose or an equivalent reproducible setup.

DBOS should run as part of the application/runtime according to the actual DBOS Rust architecture.

The application should be startable with a small number of documented commands.

Document:

```text
database initialization
migrations
RustFS initialization
application startup
test startup
```

---

# 35. Database Migrations

Create proper PostgreSQL migrations.

Do not blindly reuse SQLite migrations.

Review every migration for:

* data types
* constraints
* indexes
* foreign keys
* timestamps
* search indexes
* uniqueness
* concurrency
* defaults

If a schema change requires a data migration, implement it explicitly.

---

# 36. Preserve Existing Behavior

Before removing an existing feature, determine what behavior it provides.

Preserve externally observable behavior including:

* routes
* redirects
* authentication behavior
* authorization
* cookies
* forms
* message creation
* room behavior
* membership behavior
* search
* attachments
* rich text
* notifications
* integrations
* Cable behavior
* error behavior where practical

Do not preserve accidental implementation details.

---

# 37. Tests

Build tests around behavior and boundaries.

## PostgreSQL

Test:

* migrations
* users
* rooms
* memberships
* messages
* sessions
* attachments
* transactions
* constraints
* search

Prefer real PostgreSQL integration tests.

## Search

Test:

* normal queries
* Unicode
* punctuation
* malformed queries
* empty queries
* ranking
* pagination

## RustFS

Test:

* put
* get
* head
* delete
* presigned URLs
* metadata
* failures

Use real RustFS in integration tests where practical.

## DBOS

Test:

* successful workflows
* retry behavior
* failure recovery
* restart behavior
* idempotency
* external operation handling

## Cable

Test:

* connection
* authentication
* protocol negotiation
* subscribe
* unsubscribe
* heartbeat
* broadcast
* disconnect
* multiple subscribers

## Topcoat

Test:

* routes
* path parameters
* query parameters
* forms
* authentication
* redirects
* responses
* errors

---

# 38. Compatibility Tests

Where existing Topcamp tests describe behavior, preserve or adapt them.

Do not delete a behavioral test simply because its implementation changed.

If a test asserts an implementation detail that no longer makes architectural sense, replace it with a test of the externally observable behavior.

---

# 39. Observability

Add structured logging around:

```text
HTTP request
database transaction
DBOS workflow
workflow retry
RustFS operation
Cable connection
Cable broadcast
external integration
```

Include request/workflow IDs where appropriate.

Never log:

* passwords
* session secrets
* authentication tokens
* API keys
* private message content unless explicitly required for debugging and safely controlled

---

# 40. Performance

Do not optimize blindly.

Preserve efficient patterns where they already exist.

Pay particular attention to:

* PostgreSQL connection pooling
* query counts
* message pagination
* search indexes
* RustFS streaming
* WebSocket fanout
* DBOS workflow overhead
* HTML rendering
* allocations
* concurrent Cable subscribers

Do not introduce DBOS for tiny synchronous operations merely because it is available.

Do not introduce abstraction layers that add overhead without providing value.

---

# 41. Rust Code Quality

Use idiomatic Rust.

Requirements:

* no `unsafe` unless absolutely necessary and justified
* no `unwrap()` on recoverable runtime paths
* no `expect()` for ordinary runtime conditions
* no giant functions
* no giant state objects
* no generic "utils" dumping ground
* focused modules
* explicit error types
* clear ownership
* minimal cloning
* async only where needed
* avoid unnecessary dynamic dispatch
* avoid unnecessary allocations
* no dead compatibility abstractions

Prefer simple code over clever code.

---

# 42. Dependency Rules

Before adding a dependency:

1. verify it is actively maintained
2. verify it supports the required Rust version
3. verify it solves a real problem
4. verify whether Topcoat/Tower/Tokio/PostgreSQL/DBOS already provide the capability

Do not add another framework to compensate for not understanding Topcoat.

Do not add an ORM merely to avoid writing SQL.

Do not add a job framework because DBOS already exists.

Do not add another object storage SDK abstraction unless required.

---

# 43. Crate Structure

The existing Topcamp crate structure is not sacred.

The final structure should be optimized around actual responsibilities.

A possible structure is:

```text
crates/
  topcamp/
    domain/
    application/
    web/
    cable/
    workflows/
    storage/
    richtext/

  topcamp-db/
```

But choose the actual structure based on dependency direction and compilation boundaries.

It is acceptable to merge existing crates.

It is acceptable to delete existing crates.

It is acceptable to create new crates.

Do not split code into crates merely for aesthetic reasons.

---

# 44. Dependency Direction

Enforce this conceptual direction:

```text
web
 ↓
application
 ↓
domain
```

Infrastructure should be injected into application/domain boundaries rather than leaking framework details everywhere.

Prefer:

```text
domain ← application ← infrastructure
```

over:

```text
domain → Topcoat
domain → DBOS
domain → RustFS
```

The domain should remain portable Rust.

---

# 45. Topcoat-Specific Rule

Whenever you encounter existing code performing:

```text
routing
request extraction
query parsing
path parsing
form parsing
response construction
redirects
middleware
HTTP errors
cookie handling
connection handling
page rendering
```

stop and check Topcoat first.

Only implement custom infrastructure if Topcoat genuinely does not provide the capability.

If custom infrastructure is necessary, keep it small and integrate it with Topcoat rather than creating a parallel framework.

---

# 46. DBOS-Specific Rule

Whenever you encounter existing:

```text
job
background task
retry loop
scheduled task
webhook delivery
external API operation
notification
long-running process
```

evaluate it as a potential DBOS workflow.

But do not force ordinary synchronous code into DBOS.

The question is:

> Does this operation benefit from durable execution?

If yes, DBOS.

If no, ordinary Rust.

---

# 47. RustFS-Specific Rule

Whenever the application needs to store large binary data:

```text
do not store it in PostgreSQL
do not store it on local disk
do not build custom blob infrastructure
```

Use RustFS.

PostgreSQL stores metadata.

---

# 48. Cable-Specific Rule

Cable owns:

```text
WebSocket protocol
connection
subscription
heartbeat
fanout
realtime delivery
```

It does not own:

```text
business rules
database schema
durable jobs
blob storage
```

Keep this boundary strict.

---

# 49. Migration Order

Execute the migration in this order, but continuously compile and test.

## Step 1 — Inventory

Understand the entire repository.

Create:

```text
MIGRATION_NOTES.md
```

## Step 2 — Dependency audit

Inspect actual current Topcoat, DBOS, RustFS and relevant ecosystem APIs.

Pin versions.

## Step 3 — PostgreSQL

Implement:

* schema
* migrations
* repositories
* transactions

## Step 4 — PostgreSQL search

Implement PostgreSQL FTS and remove SQLite FTS.

## Step 5 — RustFS

Implement:

```text
BlobStore
RustFsBlobStore
```

and migrate attachments/storage.

## Step 6 — DBOS

Replace the existing job system with DBOS workflows.

Migrate integrations and durable background processing.

## Step 7 — Topcoat application layer

Replace Axum and existing HTTP plumbing.

Use Topcoat natively.

## Step 8 — Topcoat-native refactoring

Actively remove:

* redundant controllers
* redundant request wrappers
* redundant middleware
* redundant response wrappers
* redundant route abstractions
* redundant form/query/path parsing
* redundant application plumbing

## Step 9 — Cable

Reconnect Cable to:

* Topcoat
* PostgreSQL
* application/domain layer

while preserving Action Cable behavior.

## Step 10 — Integration cleanup

Remove obsolete:

* SQLite dependencies
* Axum dependencies
* old job queue
* obsolete storage code
* obsolete HTTP abstractions
* obsolete database abstractions

## Step 11 — Test and harden

Run the complete test suite.

Fix behavioral regressions.

Review concurrency.

Review failure/retry behavior.

Review security.

---

# 50. Incremental Compilation Requirement

Do not make thousands of changes without compiling.

After each major phase:

```text
cargo check
cargo test
```

Run focused tests whenever possible.

Before moving to the next major subsystem, ensure the previous subsystem is structurally sound.

---

# 51. Do Not Stop for Avoidable Problems

This is intended to be a single-shot implementation.

Do not stop and ask the user routine questions.

When encountering an implementation problem:

1. inspect the repository
2. inspect the dependency source/API
3. determine the correct architecture
4. implement the cleanest solution
5. test it
6. document important deviations

Only stop if the task is genuinely impossible without information that cannot be discovered from the repository or dependencies.

Do not stop because:

* an API differs from an example
* a dependency has changed
* a migration is larger than expected
* an old abstraction is difficult to remove
* the preferred DBOS feature is unavailable

Adapt the implementation.

---

# 52. Never Fabricate APIs

This rule is absolute.

If unsure whether an API exists:

**inspect the source.**

Do not write hypothetical code such as:

```rust
dbos.workflow(...)
```

unless that exact API exists in the selected version.

Do not assume Topcoat APIs based on names.

Do not assume RustFS APIs based on another S3 implementation.

Do not assume Tower integration signatures.

Compile against the actual dependency.

---

# 53. No Compatibility Theater

Do not produce code like:

```text
OldTopcampDatabase
    ↓
PostgresAdapter
    ↓
NewDatabase
```

if the old abstraction no longer provides value.

Likewise avoid:

```text
TopcampAxumRouter
    ↓
TopcoatAdapter
```

as a permanent architecture.

Temporary migration adapters are acceptable.

The final implementation should contain the clean architecture directly.

---

# 54. Behavioral Compatibility vs Architectural Compatibility

The implementation must preserve:

```text
URLs
authentication
authorization
message behavior
room behavior
search behavior
attachments
rich text
Cable protocol
important integrations
```

It does NOT need to preserve:

```text
crate names
module names
old traits
old controllers
old service classes
old database abstractions
old job APIs
old Axum architecture
old SQLite assumptions
```

---

# 55. Final Architecture Review

Before declaring completion, inspect the repository and ask:

### Topcoat

* Is Topcoat the actual HTTP framework?
* Is routing Topcoat-native?
* Are request/response abstractions Topcoat-native?
* Are unnecessary HTTP abstractions gone?
* Are layers implemented using Topcoat/Tower appropriately?
* Is there still accidental Axum architecture?

### PostgreSQL

* Is PostgreSQL the relational source of truth?
* Are transactions correct?
* Are indexes appropriate?
* Is search PostgreSQL-native?

### DBOS

* Are durable jobs actually DBOS workflows?
* Are workflows retry-safe?
* Are external side effects idempotent?
* Is DBOS being used only where durability is valuable?

### RustFS

* Are blobs stored in RustFS?
* Is metadata in PostgreSQL?
* Are uploads/processing resilient?

### Cable

* Is Action Cable behavior preserved?
* Is Cable independent from DBOS?
* Is realtime delivery efficient?

### Domain

* Is business logic independent of Topcoat?
* Are old framework abstractions removed?
* Are modules focused?

### Architecture

* Are there redundant abstractions?
* Are there obsolete crates?
* Are there unnecessary dependencies?
* Is there duplicated infrastructure?
* Is the code actually simpler than the original architecture?

If the answer to the final question is no, continue refactoring.

---

# 56. Definition of Done

The migration is complete only when all of the following are true:

## Framework

* [ ] Topcoat is the primary HTTP framework
* [ ] Axum is removed from the application architecture
* [ ] Topcoat routing is used
* [ ] Topcoat request/response primitives are used
* [ ] Topcoat/Tower layers are used appropriately
* [ ] redundant HTTP abstractions are removed

## Database

* [ ] SQLite is removed
* [ ] PostgreSQL is the source of truth
* [ ] transactions are correct
* [ ] migrations work from a clean database
* [ ] existing behavioral tests pass

## Search

* [ ] SQLite FTS5 is removed
* [ ] PostgreSQL FTS is implemented
* [ ] search behavior is preserved
* [ ] search indexes are present

## DBOS

* [ ] old job system is removed
* [ ] durable jobs are DBOS workflows
* [ ] workflows are retry-safe
* [ ] workflows are idempotent
* [ ] integrations use durable workflows where appropriate

## Storage

* [ ] RustFS is the blob store
* [ ] PostgreSQL contains blob metadata
* [ ] storage abstraction is small
* [ ] attachment processing is durable where appropriate

## Cable

* [ ] Action Cable protocol is preserved
* [ ] WebSocket behavior is preserved
* [ ] subscriptions work
* [ ] heartbeats work
* [ ] broadcasts work
* [ ] Cable uses the new application/database architecture

## Domain

* [ ] domain logic is framework-independent
* [ ] rich text remains a focused Rust library
* [ ] business rules are preserved
* [ ] obsolete abstractions are deleted

## Quality

* [ ] `cargo check` passes
* [ ] `cargo test` passes
* [ ] integration tests pass
* [ ] local development environment works
* [ ] configuration is documented
* [ ] secrets are not committed
* [ ] obsolete dependencies are removed
* [ ] no fabricated APIs remain
* [ ] no dead migration code remains

---

# 57. Final Architectural Rule

When deciding where code belongs, use this mapping:

```text
HTTP / routing / request / response
        → Topcoat

HTTP cross-cutting concerns
        → Topcoat / Tower layers

Business behavior
        → domain/application Rust

Relational state
        → PostgreSQL

Relational search
        → PostgreSQL FTS

Durable asynchronous execution
        → DBOS

External integrations
        → application + DBOS

Binary/object data
        → RustFS

Object metadata
        → PostgreSQL

Realtime WebSocket protocol
        → Cable / Tokio

Rich text parsing/rendering
        → Rust rich-text library
```

If code does not clearly fit one of these boundaries, reconsider the design instead of creating another abstraction.

---

# 58. Most Important Instruction

Do not think:

> "How do I migrate this Topcamp code to the new stack?"

Think:

> **"If Topcamp were being implemented today in Rust using Topcoat, PostgreSQL, DBOS, RustFS, and Tokio, how would I design it while preserving the behavior of the existing application?"**

Use the existing Topcamp repository as the **behavioral specification**.

Use Topcoat, DBOS, PostgreSQL, RustFS, and Tokio as the **architectural primitives**.

Delete anything that exists only because the old architecture required it.

The final result should not look like:

```text
old Topcamp
   +
new technologies
```

It should look like:

```text
Topcamp
reimplemented natively
around
Topcoat + PostgreSQL + DBOS + RustFS + Tokio
```

That is the objective.

