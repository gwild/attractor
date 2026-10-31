Attractor

Attractor is a content-addressed evidence and verification system written in Rust. It provides a structured foundation for collecting information, preserving provenance, resolving relationships, and evaluating whether claims are supported by verifiable evidence.

Attractor treats information as a connected body of evidence rather than a collection of independent records. Content-addressed identity allows equivalent information to converge on a shared representation, while provenance preserves the distinct observations and origins behind that information.

The system is designed around explicit data contracts, deterministic transformations, verifiable receipts, and controlled state changes.

Design principles

Content-addressed identity

Data identity is derived from content using BLAKE3 hashing. Equivalent content produces the same identity regardless of where or how it was observed.

Convergence does not discard provenance. Distinct observations remain associated with the shared identity, preserving information about their origins and the circumstances under which they were collected.

Structured relationships

Information can be connected through typed relationships in a directed graph.

The store enforces referential integrity and acyclicity: relationships cannot point to nonexistent nodes or introduce cycles. References that cannot safely become graph relationships can remain recorded as unresolved or deferred evidence.

This separates the existence of a reference from the verification and admission of the relationship it describes.

Provenance and verifiable history

Observations and transformations retain provenance. Receipts can bind derived information to source material, transformation context, and other relevant evidence.

The store maintains tamper-evident history through cryptographic hash chaining. Verification detects changes to the recorded history, including rewriting, reordering, truncation, and unauthorized additions.

As with any hash-chain system, trust in the history ultimately depends on protecting or independently anchoring its externally retained chain head.

Explicit projection boundaries

Unstructured source material is not automatically treated as trusted structured data.

Projection converts supported source material into typed representations, retaining source references and evidence such as source hashes and spans. Subsequent resolution evaluates references and determines which relationships can be admitted.

This creates distinct stages for:

* Source acquisition
* Structured projection
* Evidence and provenance recording
* Reference resolution
* Relationship materialization
* Verification and comparison

A reference extracted from a document is evidence that the reference was present; it is not, by itself, proof that the referenced object exists or that a relationship is valid.

Deterministic verification

Attractor provides mechanisms for comparing declared claims with recorded receipts and other evidence.

Verification distinguishes between supported equality, mismatch, incomplete evidence, and cases requiring further execution or evaluation. Unknown or missing information must not silently become a successful verification result.

The goal is to make verification outcomes explicit, reproducible, and inspectable rather than dependent on an agent’s interpretation or an operator’s assumptions.

Controlled state changes

Recording information and authorizing a state change are separate concerns.

Operations that materialize or publish derived information can be bounded by explicit inputs, expected state, manifests, and receipts. This allows callers to establish what was evaluated, what was changed, and whether the resulting operation satisfied its declared constraints.

Architecture

Attractor is organized as a Rust workspace with components that separate the data model, storage, transformation, resolution, and interfaces.

Component	Responsibility
attractor-core	Core data model, typed structures, and content-addressed identity
attractor-proto	Request and response types shared across interfaces
attractor-store	Persistent graph storage, relationship integrity, and verifiable history
attractor-projector	Transformation of supported source material into typed evidence
attractor-resolver	Reference resolution and relationship eligibility
attractor-assembly	Coordination of projection, resolution, and graph assembly
attractor-api	Programmatic access to Attractor operations
attractor-config	Typed configuration loading and explicit configuration requirements
attractor-io	Data exchange and input/output integration

Additional integration components can build on these foundations without changing the underlying identity, provenance, and verification model.

Verification and trust boundaries

Attractor is intended to make the basis of a verification decision inspectable.

A meaningful verification result should be traceable to:

* The exact source material or content identities evaluated
* The transformation and schema used to derive structured evidence
* The provenance associated with the evidence
* The expected claims and the fields covered by verification
* The actual receipts and comparison results
* The state against which the decision was made

A receipt is evidence, not an automatic guarantee of correctness. Consumers must validate the receipt, its provenance, its scope, and its relationship to the authoritative source state.

Likewise, a successful API response does not necessarily establish that a substantive verification receipt exists. Callers must distinguish real, validated receipts from responses that merely describe an operation or its result.

Attractor provides mechanisms for recording and checking these properties. The application consuming Attractor remains responsible for defining its authorization policies and enforcing the decisions required by its own operating environment.

Intended applications

The underlying model is applicable wherever information must be connected to its origins and evaluated against explicit constraints.

Potential applications include:

* Evidence and provenance management
* Data integration and reconciliation
* Document and knowledge graph construction
* Reproducible transformation pipelines
* Software and infrastructure verification
* Agent activity and tool-use auditing
* Contract and policy conformance
* Controlled publication and data exchange
* Long-running workflows requiring durable records

These are application areas, not assumptions about a particular deployment or workflow.

Design objective

Attractor aims to provide a dependable substrate for systems that need to establish not only what information they contain, but where it came from, how it was transformed, which relationships have been validated, and why a particular decision can be trusted.

Its central distinction is between information, evidence, verification, and authorization. They are related, but they are not interchangeable.

The result is a system in which convergence does not erase provenance, references do not automatically become facts, unknown evidence does not become success, and recorded state changes can be independently examined.
