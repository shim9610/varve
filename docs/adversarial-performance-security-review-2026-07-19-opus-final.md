# Varve Opus 수정본 성능·보안 심층 재검증

검증일: 2026-07-19  
대상 커밋: `28a1b688f9bb03b1af5a56ebdb6d333f29b426eb`  
대상 브랜치: `codex/dev-next`  
판정: **현재 상태로 릴리스 불가**

## 검증 절차

이번 평가는 이전 보고서를 그대로 재확인한 것이 아니다.

1. 1차 클린 컨텍스트 에이전트 네 명이 성능, 입력 방어, 저장 상태 전이,
   API·타입·공급망을 독립적으로 평가했다.
2. 1차 에이전트는 결론을 만들기 전에 이전 보고서와 Opus 커밋 메시지를 읽지
   않도록 지시받았다.
3. 1차 후보는
   [Opus 수정본 1차 초안](adversarial-performance-security-review-2026-07-19-opus-r1-draft.md)에
   ID별로 고정했다.
4. 완전히 다른 2차 클린 컨텍스트 팀이 각 후보를 코드, clean archive, compile
   fixture, bounded runtime test, operation counter로 다시 확인했다.
5. 2차에서 재현하지 못한 수치와 과도한 심각도는 축소하거나 미검증으로 남겼다.
6. 메인 검증에서 all-feature 전체 suite, 단독 feature Clippy, audit/deny, clean
   archive와 artifact 정리를 다시 수행했다.

보안 분류기의 오탐으로 일부 에이전트가 중단되었으나 해당 출력은 사용하지 않았다.
완료된 2차 성능, 파일 lifecycle, 전체 릴리스 감사와 별도 호환성 QA 결과만 최종
증거에 포함했다.

## 요약 결론

Opus 수정은 허위가 아니다. 다음 핵심 수정은 실제로 작동한다.

- 기존 파일의 hard-link 별칭은 native file-object lock으로 두 번째 writer가
  거부된다.
- 성공한 copy-on-write publication은 새 generation으로 rebind된다.
- rebind 실패는 typed error와 poisoned writer로 끝난다.
- 기존 self-test target은 `create_new`로 거부되어 일반적인 기존 파일 truncate가
  사라졌다.
- matrix fatal recovery finding은 기본 읽기에서 fail-closed이고 forensic access만
  명시적 opt-in이다.
- matrix sidecar는 sibling native file identity를 구분하고 임시 파일을 통한 atomic
  publication을 사용한다.
- checkpoint의 누적 **파일 bytes** O(N²)는 amortized O(N)으로 줄었다.
- typed indexed point lookup과 typed stream scan의 CRC payload 이중 읽기가 제거됐다.
- rebuild는 record마다 모든 descriptor를 순회하지 않고 tombstone key를 한 번만
  decode한다.
- process 내부 indexed reader들은 shared redb handle을 사용한다.
- stream tail lookup은 binary search이며 scalar sidecar transaction은 bounded batch다.
- `ReadLimits::UNTRUSTED`, bounded savepoint enumeration과 nominal-memory 문서가
  추가됐다.

그러나 다음 네 가지는 즉시 릴리스를 막는다.

| 우선순위 | 확정된 차단 사유 | 결과 |
| --- | --- | --- |
| Blocker | 커밋에 workspace member가 없음 | clean checkout에서 `cargo metadata`부터 실패 |
| High | computed schema hash가 encoding order 등 wire-layout 입력을 누락 | 서로 다른 bytes가 같은 schema hash를 가질 수 있음 |
| High | Windows `ReplaceFileW` 1176/1177을 prepublication 실패로 취급 | pathname이 이미 변한 상태에서 old handle을 계속 쓸 수 있음 |
| High | 동일 file object에 matrix를 재생성해도 old sidecar identity가 유지 | 이전 generation sidecar가 새 파일에 수용됨 |

Windows parent-directory flush 실패를 `Durable`로 보고하는 문제와 resident
checkpoint flush predicate의 CPU O(N²)도 PB·내구성 릴리스 전에 고쳐야 한다.

## Clean Checkout 차단

### REL-01: 커밋된 workspace가 존재하지 않는 member를 참조

**판정: 확정, Blocker.**

루트 [`Cargo.toml`](../Cargo.toml#L1)은 `tools/varve-test-runner`를 workspace member로
등록한다. 그러나 커밋 `28a1b68`에는 해당 디렉터리의 파일이 없다. 현재 로컬의
`tools/varve-test-runner/Cargo.toml`과 `src/main.rs`는 미추적 파일이다.

clean `git archive`에서 다음 명령이 즉시 실패했다.

```text
cargo metadata --no-deps --format-version 1
error: failed to load manifest for workspace member tools/varve-test-runner
```

따라서 로컬 all-feature 테스트 통과는 clean CI, clone, source package의 빌드 가능성을
증명하지 않는다. `.github/workflows/ci.yml`도 checkout 직후 같은 지점에서 실패한다.

조치:

- runner가 릴리스 인프라라면 두 소스 파일을 커밋한다.
- 아니라면 workspace, lockfile, README, test-hygiene 문서에서 참조를 제거한다.
- 수정 후 반드시 별도 clean archive에서 `cargo metadata`, `cargo package`, CI 명령을
  실행한다.

## 성능 판정

### PERF2-01: redb sidecar version retention

**판정: 축소 확정, Medium, experimental high-cardinality 경로.**

persistent savepoint가 살아 있는 dirty generation, 장기 read snapshot, `K-ever`
tombstone row가 각각 page 재사용과 logical cardinality를 유지한다. redb도 persistent
savepoint가 살아 있는 동안 사용되지 않는 page가 해제되지 않는다고 명시한다.

근거:

- [`DiskIndexStore::begin_generation`](../crates/varve-core/src/disk_index.rs#L1522)
- [`DiskIndexStore::publish_clean`](../crates/varve-core/src/disk_index.rs#L1612)
- [`DiskIndexSnapshot`](../crates/varve-core/src/disk_index.rs#L2159)
- [`historical_distinct_keys`](../crates/varve-core/src/disk_index.rs#L1416)

1차의 정확한 sidecar byte 기울기는 `max_records=1` batch 설정에 의존했고 2차에서
독립 재현되지 않았으므로 최종 수치로 채택하지 않는다. 메커니즘과 lifetime growth는
확정이지만 기본 batching의 실제 증가율은 추가 측정 대상이다.

조치: dirty-generation record/byte 경고, reader snapshot refresh/lease, `K-ever`
threshold 기반 compact/rebuild, process-wide cache budget을 제공한다.

### PERF2-02: checkpoint bytes는 선형, flush CPU는 여전히 O(N²)

**판정: 확정, High.**

[`needs_index_checkpoint`](../crates/varve-core/src/file.rs#L3848)는 매 flush마다 마지막
checkpoint를 reverse search하고 그 뒤 suffix를 다시 센다. record마다 flush하면
predicate의 누적 index 방문은 O(N²)이다.

독립 operation simulation:

| records | predicate entry touches |
| ---: | ---: |
| 1,024 | 179,392 |
| 8,192 | 11,253,678 |

입력은 8배인데 방문 수는 62.7배였다. 반면 serialized checkpoint entries는
2,582에서 20,031로 증가해 파일 bytes는 O(N)임을 확인했다.

즉 Opus 수정은 **디스크 증가량만 고쳤고 flush CPU 병목은 남겼다.**

조치: writer state에 `last_checkpoint_position`, checkpoint 이후 eligible count와
다음 geometric threshold를 O(1)으로 유지한다. reopen 시 한 번 복구하면 된다.

### PERF2-03: CRC disk-index rebuild payload 이중 traversal

**판정: 확정, High for large rebuild.**

[`rebuild_index`](../crates/varve-core/src/indexed.rs#L1120)는 scanner CRC로 payload를
읽고, [`extract_plan_update`](../crates/varve-core/src/disk_index.rs#L1037)가 indexed key
decode를 위해 같은 payload를 다시 materialize한다. 64 MiB physical payload의 첫 CRC
pass만 64 KiB read 1,024회다.

typed point lookup과 normal typed stream scan은 수정됐지만 **rebuild는 제외됐다.**
실제 physical-media read는 page cache에 따라 달라지므로 2배 disk I/O라고 단정하지
않고, logical/OS payload traversal 2회로 표현한다.

조치: scanner가 검증한 bounded payload를 extraction에 전달하거나 indexed record만
한 번 read+verify+decode하는 rebuild frame path를 만든다.

### PERF2-04: redb batch 내부 record·tail 반복 작업

**판정: asymptotic 확정, 조건부 Medium.**

[`apply_update_with_tail`](../crates/varve-core/src/disk_index.rs#L1867)은 indexed record마다
`LATEST_TABLE`을 열고 닫는다. batch begin/commit은 모든 tail을 반복 처리해
O(batches × tails) 작업을 추가한다. 실제 production 시간 비율은 독립 benchmark가
없으므로 High로 올리지 않는다.

조치: table handle을 batch lifetime 동안 유지하고, 변경된 tail만 incremental digest와
검증 대상으로 둔다.

### PERF2-05: descriptor 폭에 대한 O(D²)

**판정: 확정, Medium.**

[`validate_descriptors`](../crates/varve-core/src/disk_index.rs#L739)는 descriptor마다 format
blocks를 선형 탐색하고 generated canonicalization 후 core open에서 검증을 반복한다.
10,000 descriptor에서 두 pass는 100,010,000 비교이며 `FormatSpec::validate`의 duplicate
검사도 약 49,995,000 비교를 추가한다.

조치: sorted descriptor/block merge walk 또는 binary search를 사용하고 중복 검증을
digest/validated-plan 타입으로 제거한다.

### PERF2-06: cancellation granularity

**판정: 축소, Low policy limitation.**

scan은 record 시작 전과 시작 callback 뒤에도 cancellation을 확인한다. 다만 record
처리 중 들어온 취소는 CRC, decompression, extraction과 update가 끝난 다음 record
boundary에서 관찰된다. 문서화된 cooperative policy이므로 결함으로 보지는 않지만,
큰 record의 취소 latency는 최대 한 record 작업이다.

조치: 필요한 제품에서는 runtime record/logical limit를 낮춘다. 개선 시 CRC와 chunked
decompression loop에 byte cadence poll을 전달한다.

### PERF2-07: resident sequence sort 중복과 mmap index 메모리

**판정: 축소 확정, Medium resident-only.**

[`scan_records_from`](../crates/varve-core/src/file.rs#L6384)과
[`load_index`](../crates/varve-core/src/file.rs#L6105)가 sequence uniqueness를 각각
검증해 두 번의 N-element 복사·정렬을 수행한다. mmap도 resident index clone, set,
position 구조를 추가한다.

1차의 320 MB/million은 구조체 크기를 이용한 nominal lower-bound이며 RSS 측정이 아니다.
mapped page를 모두 resident로 간주해서도 안 된다.

조치: uniqueness 검증을 한 번만 수행하고 mmap index를 `Arc`나 sorted binary-search
구조로 공유한다. PB 파일은 scalable API를 사용한다.

### PERF2-08: indexed `flush()`가 writer gate를 유지

**판정: 축소, Low API/concurrency.**

[`VarveIndexedWriter::flush`](../crates/varve-core/src/indexed.rs#L634)는 native stream만
flush하고 pending sidecar transaction을 유지한다. `sync()`가 sidecar batch를 commit해
새 handle을 허용한다. clean visibility가 `sync()` boundary인 것은 문서화된 정책이지만,
`flush()` 후에도 gate를 잡는 동작은 불필요한 handle contention이다.

조치: clean publication과 구분되는 dirty batch commit을 flush에서 수행하거나 현재
동작을 API 이름과 generated docs에 명시한다.

### PERF2-09: unrelated sidecar open convoy

**판정: 확정, Low.**

[`open_shared_database`](../crates/varve-core/src/disk_index.rs#L1213)가 cache miss의 redb
open 동안 process-global registry mutex를 유지하므로 서로 다른 느린 sidecar open도
직렬화된다. registry 값은 `Weak`이고 dead entry를 prune하므로 registry leak 주장은
기각한다. live sidecar마다 cache가 존재하는 O(L) 메모리는 별도 capacity 문제다.

조치: identity별 initialization cell과 process-wide cache budget을 둔다.

### PERF2-10: large record append 복제

**판정: 축소, Medium bounded.**

stream preparation은 logical, stored, final-record, batch-chunk buffer를 중첩 보유할 수
있다. scalar stream은 최종 chunk copy를 하지 않으므로 1차 표현의 모든 copy가 모든
경로에 적용되지는 않는다. `BatchOptions.max_bytes`는 coalescing target이지 oversized
한 record의 hard peak-memory bound가 아니다.

조치: borrowed-or-owned payload transfer, final chunk 직접 encode, oversized record
direct write를 적용한다.

## 입력 방어·타입 판정

### DEF-01: matrix API가 block fingerprint 등록을 우회

**판정: 확정, Medium schema correctness.**

[`ensure_matrix_block`](../crates/varve-core/src/matrix.rs#L1362)은 ID, version, kind,
dimensions, category, stride를 검사하지만
[`ensure_registered_block`](../crates/varve-core/src/collections.rs#L227)을 호출하지 않는다.
같은 shape/stride와 다른 fingerprint·decode semantics를 가진 수동 matrix type으로
cell read가 성공하는 fixture가 확인됐다.

extent, alignment, slot CRC는 작동하므로 memory unsafety가 아니라 typed schema/data
confusion이다.

조치: `ensure_matrix_block` 첫 단계에서 공통 registration gate를 호출하고 same-stride
fingerprint mismatch read/write/mmap regression을 추가한다.

### DEF-02: writer limit보다 encode allocation이 먼저 발생

**판정: 축소 확정, Medium availability.**

resident push/metadata/replacement, matrix write와 generated variable nested field 일부가
unrestricted `encode_to_vec`로 값을 완성한 뒤 logical/slot limit를 확인한다. outer
scalable stream encoder 자체는 bounded이므로 모든 scalable write가 무제한이라는
1차 표현은 과장이다.

파일 mutation 전에는 실패하므로 on-disk rollback 문제보다 OOM-before-typed-error가
핵심이다.

조치: 모든 writer entry point를 `encode_to_vec_limited`로 통일하고 generated child
encoder가 parent remaining budget을 상속하게 한다. matrix는 slot stride를 encode
hard bound로 사용한다.

### DEF-03: matrix sidecar identity를 payload 처리 뒤 검사

**판정: 확정, Low bounded availability.**

[`read_matrix_sidecar_file`](../crates/varve-core/src/file.rs#L7043)은 fixed header와 extent를
읽은 뒤 payload를 allocate/read/hash하고 나서 native identity와 generation을 검증한다.
명백히 다른 sidecar도 configured sidecar/materialization limit까지 작업한 뒤 거부될 수
있다. 상한과 fallible allocation이 있어 unbounded claim은 아니다.

조치: fixed-header identity/schema/layout/requested generation을 payload allocation 전에
거부하고, category/magic처럼 작은 body prefix만 먼저 검사한다.

### DEF-04: fingerprint 문서가 authenticity를 약속한다는 주장

**판정: 기각.**

문서는 process-local collision detection과 ID/version/kind만 같은 accidental mismatch
방지를 설명하며 cryptographic authenticity를 약속하지 않는다. fingerprint가
self-attested라는 사실은 API trust boundary지만 해당 문서 표현 자체를 보안 결함으로
분류하지 않는다.

## API·호환성 판정

### API2-01: computed schema hash가 encoding order를 누락

**판정: 확정, High, release blocker.**

fixed/matrix encoding은 declaration order를 따른다
([`varve-macros`](../crates/varve-macros/src/lib.rs#L198)). 그러나
[`computed_schema_hash`](../crates/varve-core/src/format.rs#L1683)는 field descriptor를
ID 기준으로 정렬하고 ordinal을 hash하지 않는다. `BlockDescriptor`에도 per-block
endian, keyedness와 generated codec/fingerprint identity가 충분히 포함되지 않는다.

따라서 같은 field ID/name/type 집합을 다른 declaration order로 둔 두 fixed block은
computed hash가 같으면서 canonical bytes의 field order가 달라질 수 있다. safe Rust로
old file을 잘못 decode할 수 있는 wire compatibility 문제다. `schema_hash` 생략 시 0으로
비교가 꺼지는 기존 opt-in 정책은 별도 위험을 더한다.

조치:

- field encoding ordinal을 hash한다.
- block endian, keyedness, encoding mode와 generated schema identity를 포함한다.
- reorder/endian/codec-difference fixtures가 hash 또는 open을 거부하는지 고정한다.
- stable wire release에서는 기존 hash algorithm 변경에 migration/version boundary가
  필요하다.

### API2-02: self-test ownership TOCTOU

**판정: 축소, High only in concurrently writable directories.**

append self-test와 matrix claim은 기존 target을 `create_new`로 거부해 일반적인 데이터
손실은 수정됐다. 그러나 matrix self-test는 exclusive claim handle을 닫고
`create_with_dims`가 같은 pathname을 `.truncate(true)`로 다시 연다
([`diagnostics.rs`](../crates/varve-core/src/diagnostics.rs#L394),
[`file.rs`](../crates/varve-core/src/file.rs#L1655)). cleanup도 pathname을 identity 확인
없이 삭제한다 ([`diagnostics.rs`](../crates/varve-core/src/diagnostics.rs#L1009)).

동시 process가 그 사이 pathname을 교체할 수 있는 directory에서만 발생하는 race이며
bounded dynamic race는 실행하지 않았다. source-visible state gap은 확정이다.

조치: exclusive-created handle을 matrix initialization까지 유지하고, cleanup 전에
native/lock identity와 run ownership token을 비교한다.

### API2-03: keyedness contract가 일부 API에만 적용

**판정: 확정, Medium index consistency.**

[`KeyedBlockContract`](../crates/varve-core/src/traits.rs#L42)의 contradiction assertion은
`KeyedBlockVec` 경로에서만 평가된다. low-level delete, stream, indexed, merge/get 경로는
`VarveKeyedBlock`을 받으면서 `T::IS_KEYED`를 신뢰한다. 모순된 manual type이 first-seen
registration과 low-level delete를 통과하는 compile fixture가 확인됐다.

조치: 모든 keyed public entry point가 하나의 centralized compile-time/runtime contract를
반드시 평가하게 하거나, 모순 표현이 불가능한 sealed keyed marker 구조를 사용한다.

### API2-04: manual fingerprint self-attestation

**판정: 축소, Low trust-boundary limitation.**

safe public `VarveBlock` 구현자가 fingerprint를 직접 고를 수 있고 registry는 first-seen
claim을 비교한다. 이는 accidental disagreement 검출이지 의도적인 local code에 대한
신원 증명이 아니다. memory safety 문제는 확인되지 않았다.

조치: manual registration을 명시적 trusted boundary로 문서화하고, generated descriptor
identity와 manual override를 타입/API에서 구분한다.

### API2-05: renamed Cargo dependency에서 macro 실패

**판정: 확정, Medium compatibility.**

generated code가 `::varve::__core`를 hardcode한다. dependency를 `vv`로만 제공한 최소
fixture는 E0433으로 실패했다. 현재 workaround는 `extern crate vv as varve`이다.

조치: `proc-macro-crate`로 facade 이름을 찾거나 explicit `crate = path` macro option을
지원하고 renamed-dependency CI fixture를 추가한다.

### API2-06: raw matrix byte migration

**판정: 축소, Low migration footgun.**

`copy_matrix_cell_bytes_from` compatibility는 dimensions와 `SLOT_STRIDE` 중심이며 같은
크기의 endian/codec semantics를 증명하지 않는다. 다만 문서가 이 기능을 semantic
migration이 아닌 raw byte copy로 명시하고 의미 변환을 caller에게 둔다.

조치: raw 이름과 unsafe-like explicit byte-compatibility marker를 강화하고, 일반 사용자는
`VarveMigration` 경로로 유도한다.

### API2-07: CI feature matrix와 공급망 gate

**판정: 축소, Medium release assurance.**

- clean checkout은 REL-01로 어떤 CI job도 시작할 수 없다.
- all-feature union만 검사하고 개별 feature 조합을 검사하지 않는다.
- `high-cardinality-dev` 단독 Clippy는
  [`index_shared_readers.rs`](../crates/varve/tests/index_shared_readers.rs#L11)의 unused
  `std::fs` import로 실제 실패했다.
- `deny.toml`은 존재한다. 1차의 “정책 파일 없음” 주장은 기각한다.
- supply-chain job은 `continue-on-error: true`라 실패해도 CI가 성공할 수 있다.
- 일부 문서는 여전히 0.2를 기술하지만 workspace는 0.3이다.

조치: clean workspace부터 복구하고 default/no-default/각 optional feature/all-feature
matrix를 Linux와 Windows에서 실행한다. release branch의 audit/deny는 blocking으로 둔다.

## 저장·내구성 판정

### DUR2-01: `ReplaceFileW` 실패 상태 오분류

**판정: 확정, High Windows release blocker.**

[`replace_path_atomically`](../crates/varve-core/src/file.rs#L7354)은 모든
`ReplaceFileW == 0`을 publication 전 실패로 반환한다. 그러나 Microsoft는 1176에서
교체 대상 이름이 이미 사라질 수 있고, 1177에서는 두 파일의 이름·stream·attribute가
부분적으로 이동된 상태일 수 있다고 정의한다.

이 경우 caller가 temp를 삭제하고 기존 writer를 unpoisoned 상태로 유지하면 pathname과
open handle의 generation 관계가 불명확해진다. 1176/1177 local fault reproduction은 하지
못했지만 공식 API contract와 현재 exhaustive-error 처리의 모순은 확정이다.

조치:

- 1175는 documented no-mutation 상태로 별도 처리한다.
- 1176/1177은 `IndeterminatePublication` typed result로 분류한다.
- temp를 보존하고 target/replacement identity와 content를 검증한다.
- reconciliation이 끝나지 않으면 writer를 poison하고 blind retry를 금지한다.

공식 기준: [ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew).

### DUR2-02: 실패한 Windows parent sync를 `Durable`로 보고

**판정: 축소 확정, High durability-contract risk.**

[`sync_parent_directory`](../crates/varve-core/src/file.rs#L7417)은 directory를 read-only로
열고 `PermissionDenied`, `InvalidInput`, `Unsupported`를 성공으로 바꾼다. Microsoft는
`FlushFileBuffers` handle에 `GENERIC_WRITE`가 필요하다고 정의한다. 실제 flush가
거부됐는데도 `ReplaceDurability::Durable`을 반환하는 것은 계약상 잘못이다.

1차가 보고한 이 호스트의 raw AccessDenied 결과는 2차에서 독립 재현하지 못했으므로
그 수치는 제외한다. 코드가 실패를 Durable로 승격하는 사실은 확정이다.

조치: 거부/unsupported를 절대 Durable로 바꾸지 말고 `ParentSyncPending` 또는 명시적
`DirectoryDurabilityUnsupported`로 반환한다.

공식 기준: [FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers).

### DUR2-03: 동일 object matrix 재생성에서 stale sidecar 수용

**판정: 확정, High data correctness release blocker.**

matrix create는 기존 파일 object를 truncate/reinitialize한다
([`create_with_dims`](../crates/varve-core/src/file.rs#L1655)). sidecar identity는 OS
file ID/inode, schema와 동일 dimensions에서 안정적인 layout offset을 사용한다
([`matrix_native_identity`](../crates/varve-core/src/file.rs#L3177)). 같은 pathname/file
object에 같은 dimensions로 다시 만들면 이 값들이 바뀌지 않는다.

2차 전체 감사의 bounded Windows fixture에서 generation 41의 old sidecar가 재생성된
matrix에 실제로 수용됐다. sibling file identity 수정은 작동하지만 **same-object new
logical generation**을 구분하지 못한다.

조치: native matrix header/layout에 매 create마다 새 random nonce 또는 monotonic
generation UUID를 저장하고 sidecar v2/v3 identity에 포함한다. caller-supplied generation은
native creation identity를 대체할 수 없다.

### DUR2-04: redb sidecar publication의 durability state 유실

**판정: 확정, Medium.**

stream/indexed create, bootstrap, rebuild가 `replace_path_atomically`의
`ReplaceDurability`를 버리고 성공으로 반환한다
([`indexed.rs`](../crates/varve-core/src/indexed.rs#L1200),
[`stream.rs`](../crates/varve-core/src/stream.rs#L1403)). matrix/native caller는 pending
상태를 처리하지만 redb sidecar caller는 하지 않는다.

조치: `ParentSyncPending`을 typed success-with-warning 또는 error로 surface하고 이미
publish된 sidecar를 보존한다. sidecar가 재생 가능하다는 사실은 durability 오보고를
정당화하지 않는다.

### DUR2-05: first creation object-lock bind 전 truncation

**판정: 축소, Medium hypothesis with source-visible window.**

기존 file-object hard-link alias는 현재 lock으로 올바르게 막힌다. 남은 창은 target이
없을 때 서로 다른 lock path/alias가 모두 NotFound를 보고, creator가 native object를
`create + truncate`한 뒤 `bind_native`하는 순서다. loser가 bind 실패 전 새 object를
truncate할 수 있다.

Windows/Unix dangling-alias concurrent race는 동적으로 재현하지 못했으므로 확정된
data-loss라고 표현하지 않는다.

조치: create/open without truncation, object lock bind, 그 다음 `set_len(0)`과 header
write 순서로 바꾼다. public create와 create_new의 alias semantics를 별도로 고정한다.

### DUR2-06: crash temp와 lock marker 정리

**판정: 확정, Low operational hygiene.**

normal error/drop은 대부분 temp를 지우지만 process termination 뒤 native rewrite와
redb temp를 수거하는 bounded scavenger가 없다. empty `.lock` marker는 race 방지를 위해
의도적으로 남는다.

이번 all-feature suite는 13초 동안 시스템 TEMP에 92개의 0-byte `.lock` 파일을 남겼다.
이 92개는 생성 시각·크기·이름으로 정확히 선별해 제거했다. 검사 전부터 존재하던
Varve-named 파일 584개는 소유권을 단정할 수 없어 보존했다. 전체 584개 중 대부분도
empty marker지만 이전 실행·사용자 프로세스 파일일 수 있다.

조치:

- 테스트마다 owned temp directory를 사용하고 directory drop으로 native/sidecar/lock을
  함께 정리한다.
- 정상 production lock marker persistence와 test artifact cleanup을 분리한다.
- crash temp scavenging은 age, PID, exclusive object ownership과 이름 형식을 모두
  검증하는 opt-in API로 제공한다.

## 메모리 안전성과 입력 방어

이번 두 라운드에서 다음 문제는 확인되지 않았다.

- malformed file을 통한 safe Rust memory unsafety
- production parser의 hostile-header panic
- safe API를 통한 raw mmap type confusion
- matrix fatal finding의 기본 read 우회
- 기존 file hard-link를 통한 동시 writer 허용

checked arithmetic, EOF extent, compression logical length, matrix bounds, fallible reserve,
mmap membership/alignment/size/endian, unsafe raw traits의 명시적 계약은 유지된다.

이는 정형 증명은 아니다. 32-bit target, 외부 backing-file concurrent mutation, 실제
1 PiB file, long-duration allocator pressure와 모든 crash boundary는 여전히 미검증이다.

CRC는 covered-byte corruption 검출일 뿐 authenticity가 아니다. 현재 문서는 이 점을
대체로 정확히 설명한다.

## 의존성과 공급망

현재 로컬 lockfile 기준 재실행 결과:

- root `cargo audit`: 77 dependency, RustSec 1,166 advisory 기준 발견 0.
- fuzz `cargo audit`: 38 dependency, 발견 0.
- root/fuzz `cargo deny`: advisories, bans, licenses, sources 모두 통과.
- `deny.toml` 존재.
- optional zstd는 `zstd-sys` native C surface를 포함한다.

이 결과는 현재 lockfile에 알려진 advisory/policy 위반이 없다는 뜻이며 취약점 부재의
증명은 아니다. CI supply-chain job은 non-blocking이라 release gate로는 부족하다.

기준 설명: [RustSec](https://rustsec.org/).

## 기계적 검증

| 검사 | 결과 |
| --- | --- |
| `cargo fmt --all -- --check` | 통과 |
| local `cargo check --workspace --all-features --all-targets --locked` | 통과 |
| local all-feature Clippy `-D warnings` | 통과 |
| local `cargo test --workspace --all-features --locked` | 632.3초, exit 0 |
| `high-cardinality-dev` 단독 Clippy | 실패, unused `std::fs` import |
| clean archive `cargo metadata` | 실패, runner workspace member 누락 |
| root/fuzz audit·deny | 통과 |

all-feature suite에는 checkpoint growth, hard-link lock, matrix fatal/sidecar identity,
replacement state, scalable crash, compile-fail 계약이 포함됐다. 그러나 다음 probe는
ignored 상태라 이번 실행에서 제외됐다.

- one-million-key allocator/RSS stress
- 1 TiB sparse probe
- 1 PiB sparse probe

기존 문서의 1 TiB 성공과 fuzz/sanitizer 기록은 과거 증거이며 이번 2차에서 다시 실행한
것은 아니다. 실제 1 PiB 검증은 여전히 성공하지 않았다.

## 릴리스 판정

### 즉시 차단

1. missing `tools/varve-test-runner` workspace member.
2. schema hash의 field encoding ordinal/endian/codec identity 누락.
3. Windows `ReplaceFileW` 1176/1177 indeterminate state 미처리.
4. same-object matrix recreation nonce 부재로 old sidecar 수용.
5. Windows parent sync 실패를 Durable로 보고.
6. resident checkpoint flush predicate CPU O(N²).

### 릴리스 전 권장 수정

1. matrix common registration/fingerprint gate.
2. writer encode-before-limit 제거.
3. keyedness contract를 모든 public keyed entry point에 적용.
4. redb sidecar `ParentSyncPending` propagation.
5. self-test handle ownership과 identity-checked cleanup.
6. renamed dependency macro support.
7. rebuild validated payload 재사용.
8. descriptor O(D²), duplicate sequence sort와 global-open convoy 제거.
9. individual feature CI와 blocking supply-chain gate.
10. test-owned directory cleanup으로 lock marker 누적 제거.

### 수정 전 제한적 내부 사용 규칙

- clean checkout blocker를 먼저 고치지 않으면 재현 가능한 build 자체가 없다.
- stable schema를 주장하지 말고 field declaration order를 절대 변경하지 않는다.
- Windows replacement가 어떤 오류든 반환하면 writer를 폐기하고 pathname과 temp를
  별도로 확인한다.
- matrix 파일을 같은 pathname/object에 recreate할 때 기존 sidecar를 반드시 삭제하고
  application generation을 새로 만든다.
- Windows의 current `Durable` 결과를 parent-directory crash durability로 해석하지 않는다.
- `checkpoint_on_flush`와 record별 flush를 함께 사용하지 않는다.
- PB 파일은 resident API 대신 scalable stream/indexed API를 사용한다.
- CRC rebuild는 large payload에서 두 번 traversal함을 capacity plan에 반영한다.
- generated types만 사용하고 manual matrix/block/keyed 구현은 금지한다.
- self-test는 접근권한이 제한된 전용 임시 directory에서만 실행한다.
- untrusted input에는 `ReadLimits::UNTRUSTED`와 외부 process memory limit를 함께 적용한다.

## 최종 평가

Opus 수정은 이전 보고서의 다수 결함을 실질적으로 고쳤다. 특히 existing-object
single-writer lock, successful replacement state, matrix fatal gate, typed CRC read,
shared indexed reader와 checkpoint byte growth는 이전보다 명확히 좋아졌다.

하지만 현재 커밋은 clean checkout조차 빌드되지 않으며, schema compatibility와 matrix
sidecar generation, Windows replacement/durability 계약에 새로운 release blocker가
남아 있다. checkpoint 문제도 파일 크기만 선형화됐을 뿐 flush predicate CPU는 여전히
O(N²)다.

따라서 현재 상태는 다음과 같이 판정한다.

- **공개 릴리스:** 불가.
- **wire/API stable 선언:** 불가.
- **통제된 내부 시험:** 위 제한을 강제하고 clean workspace를 먼저 복구하면 가능.
- **PB-scale 주장:** scalable 구조와 일부 bounded evidence는 유효하지만 전체 경로와
  실제 PiB 검증은 아직 부족.
- **safe memory-safety 상태:** 확인된 결함 없음. 다만 데이터 정확성, 내구성,
  가용성·성능 결함은 위와 같이 존재함.

## 공식 외부 기준

- [Microsoft ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)
- [Microsoft FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)
- [redb persistent savepoint semantics](https://docs.rs/redb/latest/redb/struct.WriteTransaction.html#method.persistent_savepoint)
- [RustSec](https://rustsec.org/)
