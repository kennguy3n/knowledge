# Knowledge for Education

> **TL;DR:** Education combines strict student-privacy law (FERPA,
> COPPA) with low-connectivity schools and multilingual classrooms.
> Knowledge's on-device, offline-first model keeps student data out of
> central servers, works without reliable internet, and extracts across
> 22 languages.

## The Business Problem

An education technology company builds an AI study and tutoring
assistant for schools. Three realities shape what is acceptable:

1. **Student privacy is heavily regulated.** In the US, FERPA governs
   education records and COPPA restricts data collection from children
   under 13; other jurisdictions have their own student-data rules.
   Routing minors' data to third-party AI services is fraught — and
   often contractually or legally prohibited by the school district.

2. **Connectivity is unreliable.** Many schools — especially in rural
   or under-resourced areas — have intermittent or poor internet. A
   cloud-dependent assistant simply stops working during a class.

3. **Classrooms are multilingual.** Students learn in multiple
   languages and dialects; an English-only tool fails a large share of
   learners.

A product that requires shipping children's data to the cloud, over a
reliable connection, in English, fails on all three counts.

## The Technical Approach

Knowledge's defaults are unusually well-suited to the education
profile:

- **On-device keeps student data local** ([post 1](01-why-on-device-memory.md)).
  Education records and a student's interaction history stay on the
  device, not on a vendor's servers. This shrinks the FERPA/COPPA
  surface dramatically: there is no central store of minors' data to
  secure, and no third-party AI vendor ingesting it for routine memory
  and retrieval. The [compliance doc](../docs/operator/compliance.md)
  discusses these considerations.
- **Offline-first.** Because retrieval, extraction, and synthesis run
  on-device ([posts 2](02-multilingual-extraction-engine.md),
  [5](05-on-device-inference-under-constraints.md),
  [8](08-performance-at-device-scale.md)), the assistant keeps working
  when the internet doesn't. No connectivity dependency for the core
  experience means a dropped connection mid-lesson is a non-event.
- **Multilingual by default** ([post 2](02-multilingual-extraction-engine.md)).
  Extraction works across 22 languages with per-sentence detection, so
  a multilingual classroom is supported out of the box rather than
  via a separate localized build.
- **Cryptographic forgetting** ([post 3](03-memory-that-forgets.md))
  for end-of-year data disposal or a parent's deletion request —
  erase the student's scope and the data is unrecoverable.
- **Device-tier inference** ([post 5](05-on-device-inference-under-constraints.md))
  so the assistant runs on the inexpensive devices schools actually
  buy, degrading gracefully on the lowest tiers.

## Implementation Walk-through

A per-student scope keeps each learner's data isolated and disposable:

```text
scope_id = student_scope(student_id)      // one scope per student
ingest_message(scope_id, work, ...)       // stays on-device, encrypted
query(scope_id, "where did I struggle last week?")  // works offline
forget(scope_id)                          // end-of-year / parental request
```

Because the experience is offline-first, the school does not need to
provision reliable bandwidth or a backend for the core feature; an
optional [hybrid deployment](07-zero-to-production-deployment.md) can be
added in-district if connectors or central administration are needed,
while keeping student content within the school's boundary. (Compliance
with FERPA/COPPA is a property of the district's full program; Knowledge
supplies the technical controls.)

## Performance & Cost Implications

On-device retrieval at ~9.7 ms ([post 8](08-performance-at-device-scale.md))
keeps the assistant responsive on classroom hardware, and multilingual
extraction runs at consistent throughput across languages
([post 2](02-multilingual-extraction-engine.md)).

Cost is decisive in the education market, which is chronically
budget-constrained. On-device operation means $0 marginal
infrastructure cost ([post 10](10-cost-engineering-zero-marginal.md)) —
a district can deploy to thousands of students without a per-student
cloud bill, and without buying the reliable connectivity a cloud
assistant would demand.

## What's Next

Education and APAC show the substrate's fit for constrained, privacy-
sensitive environments. The final post zooms out to the deployment that
ties the series together: the no-ops setup for a small-to-medium
business that just wants the benefits without running infrastructure.

---
*This is part 17 of the "Building Knowledge" series. [Previous: Knowledge Across APAC](16-knowledge-across-apac.md) | [Next: Knowledge for SMB](18-knowledge-for-smb.md)*
