# Varve 성능·보안 심층 검증 보고서

검증일: 2026-07-19  
대상: `codex/dev-next`의 현재 dirty working tree  
상태: 1차 적대적 평가 후, 별도의 클린 컨텍스트 다중 에이전트가 코드와 제한된 재현 시험으로 재검증한 최종본

## 결론

현재 Varve에서 안전한 Rust API를 통한 메모리 안전성 파괴나 악성 바이트 입력에
의한 panic은 확인되지 않았다. 오프셋 산술, EOF 범위, 압축 해제 크기, matrix 범위,
mmap 정렬·크기·타입 조건에는 방어가 광범위하게 들어가 있다.

그러나 **일반 공개 릴리스 또는 wire/API 안정화 선언을 하기에는 이르다.** 다음
데이터 보존·무결성 결함은 릴리스 차단 항목이다.

| 우선순위 | 확정된 문제 | 영향 |
| --- | --- | --- |
| Critical | 경로별 `.lock` 때문에 hard link 별칭으로 single-writer를 우회할 수 있음 | 두 writer가 모두 sequence 1을 기록했고 재오픈이 duplicate sequence로 실패함 |
| High | atomic rename 성공 후 parent sync 오류가 나면 writer가 새 generation에 rebind/poison되지 않음 | 오류 뒤 기존 handle에 계속 쓴 데이터가 pathname에서 보이지 않을 수 있음 |
| High | `Format::self_test(path).run()`이 기존 파일을 truncate하고 cleanup 시 삭제할 수 있음 | 안전한 공개 API에서 호출자 파일 손실 가능 |
| Medium | matrix metadata CRC가 `Fatal`이어도 layout과 cell read가 계속 가능함 | 복구 보고서를 확인하지 않은 안전한 읽기가 fatal 상태를 소비함 |
| Medium | matrix sidecar가 특정 native 파일 identity에 묶이지 않고, publication도 in-place truncate 방식임 | 같은 규격의 다른 파일 sidecar가 수용되며 crash 중 부분 sidecar 노출 가능 |
| Medium | 수동 `VarveBlock`이 같은 ID/version/kind로 등록 타입을 가장할 수 있음 | 안전한 API에서 schema/codec 혼동 가능 |
| Medium | 수동 `VarveKeyedBlock`과 `IS_KEYED=false`를 동시에 선언할 수 있음 | keyed offset-chain 갱신을 우회해 index 일관성을 훼손할 수 있음 |

PB/high-cardinality 목표를 직접 저해하는 확정 성능 문제도 있다. 가장 큰 것은
resident `checkpoint_on_flush`의 누적 O(N²) 쓰기, tombstone rebuild의 O(records ×
descriptors) payload I/O, CRC typed lookup/scan의 payload 이중 읽기, 그리고 독립 indexed
reader끼리도 발생하는 redb `IndexBusy`이다.

현재 코드는 제한된 내부 배포에는 사용할 수 있지만, 아래의 임시 운용 조건을 모두
지켜야 한다. 공개 안정화 전에는 릴리스 차단 항목을 코드로 고쳐야 한다.

## 검증 방법

1차 팀은 성능, hostile-input, 충돌복구, API·매크로·의존성 네 축에서 최대한
비관적으로 후보 결함을 만들었다. 그 결과는 단정하지 않고
[1차 초안](adversarial-performance-security-review-2026-07-19-draft.md)에 ID별로
고정했다.

2차 팀은 1차 대화 컨텍스트를 받지 않은 새 에이전트들로 구성했다. 각 에이전트는
초안의 주장을 신뢰하지 않고 현재 코드, 테스트, 문서, 제한된 `C:\tmp` 재현으로
각 항목을 `확정`, `축소`, `기각`, `미검증`으로 판정했다. 재현 파일은 모두
정리되었고 repository 파일은 수정하지 않았다.

현재 wall-clock 성능 비교는 하지 않았다. PID 18192의 원인 불명 `find.exe`가 논리
코어 하나를 지속 사용하고 있어 기존 기준선과 비교할 수 없기 때문이다. 대신 호출
경로, commit 수, OS read-transfer bytes, 파일 크기 기울기와 row count를 사용했다.

## 성능 판정

### PERF-01: scalable stream scalar append의 record당 sidecar commit

**판정: 축소 확정, High.** 최초 주장을 모든 indexed API에 일반화한 것은 틀렸다.

- scalable **stream** scalar `push_*`는 prepared chunk 하나마다 sidecar batch를
  commit하고 redb `Durability::Immediate`를 사용한다.
- scalar 4회는 native write 4회, sidecar commit 4회였다. plural batch는 각각
  1회였다.
- scalable **indexed** scalar append는 최대 16,384건 또는 `sync()`까지 sidecar
  transaction을 유지한다. 4회 append 뒤 sidecar commit은 1회였다.

근거: [`stream.rs`](../crates/varve-core/src/stream.rs#L943),
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs#L1282),
[`indexed.rs`](../crates/varve-core/src/indexed.rs#L23). redb는
`Durability::Immediate` transaction이 commit 반환 시 영속화된다고 정의한다.

조치: generated plural/iterator API를 기본 예제로 사용한다. stream scalar 경로도
bounded transaction을 유지하고 `chunk_records` 또는 `sync()`에서 commit하도록
통합한다.

### PERF-02: `checkpoint_on_flush` 누적 O(N²)

**판정: 확정, High.** resident `flush()`마다 현재 전체 index를 새 checkpoint로
직렬화하고, open은 checkpoint들을 다시 스캔·검증한다.

| records | 마지막에 한 번 flush | record마다 flush |
| ---: | ---: | ---: |
| 8 | 980 B | 5,446 B |
| 16 | 1,884 B | 20,214 B |
| 32 | 3,692 B | 77,782 B |

측정값은 각각 `76 + 113N`과 `22 + 94N + 73N²`에 정확히 일치했다. scalable stream은
이 정책을 거부하므로 해당 경로에는 적용되지 않는다.

근거: [`file.rs`](../crates/varve-core/src/file.rs#L2409),
[`file.rs`](../crates/varve-core/src/file.rs#L3550),
[`stream.rs`](../crates/varve-core/src/stream.rs#L1104).

조치: growing append 파일에서 record별 flush/checkpoint를 금지한다. 근본 수정은
wire-versioned delta checkpoint이며, 빈 checkpoint 억제만으로 해결되지 않는다.

### PERF-03: resident API의 O(records) open과 메모리

**판정: 축소 확정, 의도된 제한.** `VarveFile`은 open 시 전체 native record를
스캔하고 `Vec<RecordIndexEntry>`를 보유한다. 다만 payload는 lazy decode이고 keyed
map도 `keyed_blocks()` 호출 시 만들어지므로 “open이 모든 객체와 key map을 보유한다”는
1차 표현은 과장이었다.

근거: [`file.rs`](../crates/varve-core/src/file.rs#L930),
[`file.rs`](../crates/varve-core/src/file.rs#L5877),
[`scalable-io.md`](scalable-io.md#resident-and-scalable-surfaces).

조치: PB 파일에는 feature-gated stream/indexed API만 사용한다. 생성자와 generated
API 이름·문서에서 resident 성격을 더 눈에 띄게 표시한다.

### PERF-04: sidecar가 live key가 아니라 historical distinct key에 비례

**판정: 확정, high-churn에서 High.** tombstone은 redb row를 삭제하지 않고 latest
table 값을 대체한다. insert/delete 3주기에서 live key는 0이었지만 row는
`64 → 128 → 192`로 증가했고, 384 record rebuild 뒤에도 192 row였다. 물리 파일은
redb page 재사용 때문에 단조 증가하지 않지만 논리 cardinality는 `K-ever`이다.

근거: [`disk_index.rs`](../crates/varve-core/src/disk_index.rs#L1551).

조치: historical distinct keys를 capacity metric으로 노출한다. reclaim은 native
compact와 sidecar rebuild를 함께 수행해야 하며 tombstone row만 즉시 삭제하면 안 된다.

### PERF-05: rebuild의 O(records × descriptors) tombstone decode

**판정: 확정, High.** rebuild가 record마다 모든 descriptor를 순회한다. 특히
tombstone key는 descriptor마다 전체 decode될 수 있다.

8개의 65,536문자 tombstone에서 descriptor 1개는 2,061,267 OS read-transfer bytes,
8개는 5,732,403 bytes였다. 차이 3,671,136 bytes는 `7 × 8 × 65,556`과 일치한다.

근거: [`indexed.rs`](../crates/varve-core/src/indexed.rs#L1082),
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs#L1012).

조치: block ID로 descriptor를 한 번 찾고 key를 한 번만 decode한다. 이미 존재하는
`DiskIndexPlan::descriptor` binary search를 재사용할 수 있다.

### PERF-06: CRC typed point lookup과 streaming scan의 payload 이중 읽기

**판정: 확정.** indexed point lookup은 Medium, payload 중심 full scan은 High이다.

- 4 MiB point lookup: integrity none 4,194,416 B, CRC 8,388,832 B.
- 4 MiB streaming typed scan: integrity none 4,194,392 B, CRC 8,388,784 B.

두 경로 모두 scanner/entry 검증 후 typed decode를 위해 payload를 다시 읽는다.
이는 page cache hit 여부와 무관하게 OS read-transfer 기준 정확히 2배였다.

근거: [`indexed.rs`](../crates/varve-core/src/indexed.rs#L154),
[`indexed.rs`](../crates/varve-core/src/indexed.rs#L270),
[`stream.rs`](../crates/varve-core/src/stream.rs#L420),
[`file.rs`](../crates/varve-core/src/file.rs#L6629).

조치: bounded validated payload를 typed decode에 그대로 넘긴다.

### PERF-07: indexed handle 간 `IndexBusy`

**판정: 확정, Medium.** indexed reader도 redb를 writable open한다. writer-reader뿐
아니라 독립 reader 두 개도 동시에 열리지 않았고 두 번째는 `IndexBusy`였다. resident
reader에는 해당하지 않는다.

근거: [`disk_index.rs`](../crates/varve-core/src/disk_index.rs#L2125),
[`indexed.rs`](../crates/varve-core/src/indexed.rs#L1697).

조치: 현재는 indexed handle ownership을 직렬화한다. 최소 개선은 process-local
shared database handle coordinator이며, 장기적으로 read-only snapshot handle을 제공한다.

### PERF-08: block-tail 유지의 O(records × blocks)

**판정: 확정, 조건부 Medium.** `block_offset_chain`이 켜지면 sorted tail vector에서
`previous_block`과 `set_tail`이 선형 검색한다. sidecar transaction도 모든 tail을
읽고 검증·digest한다. block chain을 끄면 vector가 비어 있다.

근거: [`stream.rs`](../crates/varve-core/src/stream.rs#L1093),
[`stream.rs`](../crates/varve-core/src/stream.rs#L1180),
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs#L1665).

조치: sorted vector binary search와 chunk별 최종 tail collapse를 적용한다.

## 보안·방어 판정

### 확인되지 않은 것

- safe API memory unsafety는 확인되지 않았다.
- hostile header/payload로 production parser panic은 확인되지 않았다.
- mmap/raw zero-copy는 unsafe API와 unsafe trait로 경계가 드러나며, extent,
  membership, kind, endian, exact size, alignment, `FromBytes` 검사가 존재한다.
- 외부 process가 mapping 중 파일을 mutate/truncate하지 않는다는 보장은 여전히
  unsafe caller의 책임이다.

이는 안전성의 증명은 아니다. 32-bit target, 외부 동시 mutation, 실제 1 PiB 파일,
거대한 redb savepoint 집합은 충분히 시험되지 않았다.

### SEC-01: 기본 aggregate limit가 사실상 무제한

**판정: 축소, Medium availability 정책.** `ReadLimits::STANDARD`는 한 record payload,
logical payload, materialization, matrix/mmap 등의 단위 상한은 유한하지만 총 file
length, record, scan bytes, resident index bytes, segment count는 `u64::MAX`이다.

이것은 append 파일 자체의 wire 상한이 아니며, 사용자가 요구한 무한 성장 모델과
충돌하지 않는다. 문제는 untrusted 대형 파일을 resident open할 때 호출자가 유한
runtime 정책을 선택하지 않으면 CPU·I/O·메모리를 공격자가 정할 수 있다는 점이다.

근거: [`format.rs`](../crates/varve-core/src/format.rs#L96),
[`api-reference.md`](api-reference.md#runtime-limits).

조치: 포맷에 compile-time 총량 상한을 복원하지 않는다. 대신 untrusted input용
finite runtime preset을 제공하고, 큰 정상 파일은 scalable API로 처리한다.

### SEC-02: redb sidecar 총 길이와 savepoint 열거

**판정: 축소, Low.** paged `.vki/.vks` 전체 길이에 `max_sidecar_len`을 적용하지 않는
것은 PB 설계상 의도되고 테스트·문서화되어 있다. 따라서 1차의 “계약 위반 High”는
기각한다. 다만 safe open이 persistent savepoint를 모두 `count()`하여 Varve 차원의
개수 상한이 없다.

근거: [`scalable-io.md`](scalable-io.md#resource-limits),
[`indexed.rs`](../crates/varve-core/src/indexed.rs#L1674),
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs#L1967).

조치: 두 개 이상인지 판단하는 데 필요한 만큼만 열거한다. `max_sidecar_len`의
적용 범위를 API 이름·주석에서 명확히 한다.

### SEC-03: allocation budget는 peak RSS가 아님

**판정: 축소 확정, Medium availability.** 현재 budget은 logical/nominal bytes를
세며 container bucket/node, sequence uniqueness의 추가 `8 × N`, mmap index와 map
복제, keyed materialization의 보조 map 전체를 peak RSS로 보장하지 않는다. checked
arithmetic와 fallible reserve는 방어로 작동한다.

근거: [`file.rs`](../crates/varve-core/src/file.rs#L6477),
[`codec.rs`](../crates/varve-core/src/codec.rs#L319),
[`collections.rs`](../crates/varve-core/src/collections.rs#L11).

조치: 문서와 타입 이름에서 nominal accounting임을 명시하고, 알려진 임시 복제는
budget에 포함한다. hard peak RSS 보장은 별도 process/cgroup/job 정책으로 둔다.

### SEC-04: CRC의 범위

**판정: 정상 정책.** CRC32는 covered-byte corruption 검출이며 인증·출처·replay
방어가 아니다. 문서는 대체로 이를 정확히 말한다. `spec.md` acceptance criterion의
“tampering” 한 곳은 “covered-byte modification”으로 고치는 편이 정확하다.

외부 공격자에 대한 진본성은 signature/MAC과 별도 replay policy를 호출자가
제공해야 한다.

### SEC-05: matrix metadata `Fatal`이 read를 막지 않음

**판정: 확정, Medium integrity.** metadata CRC mismatch가 recovery report의
`Fatal`로 기록되어도 `read_layout_at_len`은 layout을 만들고 cell read는 fatal
finding을 gate하지 않는다. 구조적 extent, slot CRC, commit-map quarantine은 별도로
작동하지만 `Fatal` 의미와 안전한 API 동작이 불일치한다.

근거: [`matrix.rs`](../crates/varve-core/src/matrix.rs#L551),
[`matrix.rs`](../crates/varve-core/src/matrix.rs#L753),
[`matrix.rs`](../crates/varve-core/src/matrix.rs#L2369).

조치: fatal finding이 있으면 open/access를 실패시키거나, 계속 읽을 수 있는 finding은
fatal보다 낮은 등급과 명확한 opt-in API로 바꾼다.

### SEC-06: fuzz 범위

**판정: 테스트 공백.** sidecar fuzz harness는 입력을 1 MiB로 자르고 operation/record
수를 제한한다. 지금까지 artifact가 없다는 사실은 유효하지만 huge/sparse redb,
대규모 savepoint, allocator pressure, 32-bit, 외부 backing-file mutation을 대변하지
않는다.

근거: [`sidecar.rs`](../fuzz/src/sidecar.rs#L10),
[`fuzzing-and-fault-injection.md`](fuzzing-and-fault-injection.md).

## 충돌복구·동시성 판정

### DUR-01: rename 이후 sync 오류 상태

**판정: 확정, High.** `replace_path_atomically`은 rename/`ReplaceFileW` 후 parent sync를
한다. 이 sync가 무시 대상이 아닌 오류를 반환하면 caller는 rebind/poison 전에
빠져나간다. pathname은 이미 새 generation을 가리킬 수 있으므로 “실패 시 원본
불변”이 아니다.

근거: [`file.rs`](../crates/varve-core/src/file.rs#L6964),
[`file.rs`](../crates/varve-core/src/file.rs#L3219),
[`durability-model.md`](durability-model.md).

임시 규칙: replacement가 어떤 오류든 반환하면 writer를 폐기하고 pathname을 다시
연다. 수정: post-publication 오류를 typed outcome으로 분리하고 rebind 또는 poison을
무조건 수행한다.

### DUR-02: path alias single-writer 우회

**판정: 확정, Critical.** lock path는 native path에 `.lock`을 붙여 만든다. Windows
hard link 두 경로로 writer 두 개가 동시에 열렸고, 둘 다 sequence 1을 쓰고 sync에
성공했다. 이후 reopen은 duplicate native record sequence로 실패했다. scalable path의
canonicalize도 hard link를 하나의 이름으로 합치지 못한다.

근거: [`file.rs`](../crates/varve-core/src/file.rs#L7121),
[`file.rs`](../crates/varve-core/src/file.rs#L7451),
[`layout.rs`](../crates/varve-core/src/layout.rs#L759).

임시 규칙: 한 파일에 하나의 canonical application path만 사용하고 hard link/reparse
alias를 금지하며 외부 singleton writer를 둔다. 수정: 열린 native file object 자체에
대한 OS lock을 권위 있는 lock으로 사용하고 path marker는 진단용으로만 유지한다.

### DUR-03: rebuild 중 pathname generation 교체

**판정: 축소, Medium availability.** rebuild는 retained snapshot을 읽은 뒤 최종
observer 이후 pathname identity를 다시 확인하지 않고 sidecar를 publish한다. 다만
sidecar identity에 Unix inode/device 또는 Windows volume/file ID가 있어 다음 open은
stale sidecar를 거부한다. 잘못된 데이터 제공보다는 성공 오보고와 불필요한 재구축
문제다. Unix pathname swap 자체는 이 Windows 검증에서 실행하지 못했다.

같은 누락은 `bootstrap_stream_checkpoint_with_progress`에도 보인다.

근거: [`indexed.rs`](../crates/varve-core/src/indexed.rs#L1039),
[`stream.rs`](../crates/varve-core/src/stream.rs#L126),
[`stream.rs`](../crates/varve-core/src/stream.rs#L1319).

조치: publish 직전에 pathname identity와 retained snapshot identity를 다시 비교한다.

### DUR-04/05: matrix sidecar identity와 publication

**판정: 확정, Medium.** manifest는 format/schema/category/caller generation/length/CRC를
검증하지만 native OS object나 matrix runtime dimension identity는 갖지 않는다.
generation 7인 파일 A의 sidecar를 같은 규격·generation 7의 파일 B가 실제로
받아들였다.

또한 `write_matrix_sidecar_file`은 `File::create`로 기존 sidecar를 truncate하고
그 자리에서 쓴 뒤 `sync_all`만 한다. atomic rename, parent sync, sidecar lock,
native/sidecar commit ordering이 없다.

근거: [`file.rs`](../crates/varve-core/src/file.rs#L102),
[`file.rs`](../crates/varve-core/src/file.rs#L6713),
[`file.rs`](../crates/varve-core/src/file.rs#L6906).

조치: native fingerprint와 matrix-layout generation을 sidecar v2에 넣는다. 같은
디렉터리의 RAII temp를 sync하고 atomic replace한 뒤 parent sync하며, native와
sidecar의 ordered commit API를 제공한다.

### DUR-06/07/08: 아직 조건부인 영역

| ID | 판정 | 확인된 사실 | 남은 검증 |
| --- | --- | --- | --- |
| DUR-06 | 조건부 Medium | ordinary create/sync는 file만 sync하고 parent directory를 sync하지 않음 | Unix hard-power-loss에서 새 directory entry 보존 |
| DUR-07 | 축소 Medium availability | indexed rebuild는 RAII temp, native rewrite temp는 PID 이름과 정상 오류 수동 삭제, startup scavenger 없음 | process abort 뒤 실제 full-temp 잔류량 |
| DUR-08 | 미검증 Low | writer marker write/clear는 flush만 사용, OS lock이 safety를 지킴 | power loss 후 marker resurrection |

## 공개 API·매크로 판정

### API-01: self-test의 파괴적 target 처리

**판정: 확정, High data loss.** 안전한 `FormatSelfTest::run`이 caller path를
`VarveFile::create`로 truncate한다. sentinel은 29 bytes에서 23 bytes로 바뀌었고
`report_passed=true`, `preserved=false`였다. `.cleanup(true)`는 그 path까지 삭제한다.
상세 self-check 문서는 임시 path와 truncation을 경고하지만 API 자체의 안전성 문제를
없애지는 않는다.

근거: [`diagnostics.rs`](../crates/varve-core/src/diagnostics.rs#L380),
[`file.rs`](../crates/varve-core/src/file.rs#L1547),
[`self-check-guide.md`](self-check-guide.md#end-to-end-self-test).

조치: `create_new`를 사용하고 현재 run이 생성·소유했음을 증명한 파일만 cleanup한다.

### API-02: 등록 block type 가장

**판정: 확정, Medium schema/data confusion.** safe manual `VarveBlock`은 동일한
ID/version/kind만 맞추면 등록 descriptor를 통과한다. field와 codec/type identity는
비교하지 않는다. 메모리 안전성 결과는 확인되지 않았지만 안전한 typed read/write가
잘못된 schema로 동작할 수 있다.

근거: [`traits.rs`](../crates/varve-core/src/traits.rs#L5),
[`collections.rs`](../crates/varve-core/src/collections.rs#L220).

조치: generated type만 임시 지원 대상으로 제한한다. 구현은 immutable block schema
fingerprint를 trait/descriptor에 넣고 registration 때 비교한다.

### API-03: keyedness 모순

**판정: 확정, Medium index consistency.** `VarveKeyedBlock` 구현과
`VarveBlock::IS_KEYED=false`가 동시에 가능하다. runtime 재현에서
`implements_keyed=true`, `declared_is_keyed=false`, `push_allowed=true`였다. derive는
정확히 생성하고 feature 사용 시 상수 생략도 compile-fail이지만, 거짓 값 자체는
막지 않는다.

근거: [`traits.rs`](../crates/varve-core/src/traits.rs#L35),
[`stream.rs`](../crates/varve-core/src/stream.rs#L699).

조치: keyedness를 registry가 소유하고 `T::IS_KEYED`와 대조하거나, 서로 모순될 수
없는 sealed marker 계층으로 바꾼다.

### API-04/05/06

| ID | 판정 | 내용 |
| --- | --- | --- |
| API-04 | 축소, Low | DSL에서 `schema_hash` 생략 시 0이며 native open 비교를 끈다. 선택 기능이지만 생략 의미를 더 명확히 하거나 breaking release에서 computed를 기본으로 해야 함 |
| API-05 | 확정, Low | macro가 `::varve`를 hardcode하여 dependency rename compile fixture가 E0433으로 실패함. `proc_macro_crate`로 facade 이름을 찾아야 함 |
| API-06 | 방어 확인 | unsafe mmap/raw 경계와 런타임 검사가 있으며 8개 zero-copy test가 통과함. safe soundness 결함은 발견하지 못함 |

## 의존성과 공급망

현재 snapshot에서 다음 검사는 통과했다.

- root `cargo audit`: 77 dependencies, 1,166개 advisory DB 기준 발견 0.
- fuzz lockfile `cargo audit`: 38 dependencies, 발견 0.
- root/fuzz `cargo deny`: advisories, bans, licenses, sources 모두 통과.
- Git source dependency 없음.
- 중복은 test/build graph의 `getrandom` 0.3.4와 0.4.3뿐이다.
- optional zstd는 `zstd-sys` native C surface를 포함한다.

RustSec가 설명하듯 `cargo-audit`은 `Cargo.lock`을 알려진 취약점 DB와 대조하고,
`cargo-deny`는 advisory 외에 license/source/ban 정책도 검사한다. 이 결과는 현재
lockfile에 알려진 문제가 없다는 뜻이지, 취약점 부재의 증명은 아니다.

## 기존 테스트 증거의 한계

확인된 기계적 증거:

- hostile-input/security focused tests 84개 통과.
- compile/API/high-cardinality/policy/self-check focused tests 32개 통과.
- 이전 전체 crash matrix 510.96초 통과.
- 네 ASan fuzz target 각 30초, strict Miri 3개, all-feature core ASan gate 통과.
- sidecar fuzz 7,195 executions, retained artifact와 sanitizer finding 0.
- 1 TiB sparse typed/CRC probe 통과, 실제 할당량 65,536 bytes.
- test session artifact는 성공 시 정리되며 이번 2차 검증 temp도 모두 제거됨.

해석 제한:

- fuzz 실행 시간과 input cap 때문에 parser 완전성 증명이 아니다.
- 실제 1 PiB sparse file은 NTFS error 87로 생성되지 않아 검증하지 못했다.
- accepted 1M-key 수치는 한 번의 warm/sequential 관측이며 통계적 benchmark가 아니다.
- 현재 외부 CPU 부하 때문에 새 wall-clock 비교를 하지 않았다.
- repository에 `.github` CI workflow가 없어 이 gate들이 매 변경마다 자동 강제되지 않는다.

## 현재 기준선

2026-07-17에 수용한 release-mode 1M unique composite key 기준선은 다음과 같다.
현재 환경에서는 재측정하지 않았으므로 회귀 결론에 사용하지 않는다.

| 항목 | 값 |
| --- | ---: |
| append | 4.072 s |
| final sync | 116.2 ms |
| reopen | 9.35 ms |
| 10,000 warm lookup | 141.5 ms |
| native | 128,000,026 B |
| sidecar | 134,746,112 B |
| peak allocator delta | 12,346,781 B |
| native write calls | 62 |

## 릴리스 권고

### 지금 바로 고칠 순서

1. native file-object 기반 single-writer lock과 hard-link regression test.
2. replacement post-publication error의 typed state, rebind/poison 보장.
3. self-test `create_new`와 ownership 기반 cleanup.
4. matrix fatal CRC fail-closed 동작.
5. matrix sidecar identity v2와 atomic ordered publication.
6. manual block schema fingerprint와 keyedness invariant.
7. CRC validated payload 재사용, tombstone descriptor 단일 lookup.
8. checkpoint delta 설계와 indexed shared-read handle.
9. CI에 fmt, Clippy, all-feature test, deny/audit, Miri/ASan/fuzz smoke를 고정.

### 수정 전 제한적 내부 사용 규칙

- generated block/format API만 사용하고 manual `VarveBlock`/`VarveKeyedBlock`은 금지한다.
- `self_test`에는 반드시 충돌 불가능한 새 임시 경로만 전달한다.
- 하나의 canonical application path만 사용하고 hard link/reparse alias를 금지한다.
- writer singleton을 application/OS 수준에서 추가로 보장한다.
- replacement가 오류를 반환하면 해당 writer를 즉시 폐기하고 pathname을 재오픈한다.
- high-cardinality 파일은 plural/iterator batch와 scalable indexed/stream API를 쓴다.
- growing append 파일에서 `checkpoint_on_flush`를 사용하지 않는다.
- indexed handle은 한 번에 하나만 연다.
- matrix sidecar는 authoritative 상태로 쓰지 않거나 application이 identity와 atomic
  publication을 별도로 보장한다.
- untrusted 입력은 호출 시점 finite limits를 지정하고 resident open을 피한다.
- CRC를 인증으로 취급하지 않고 외부 signature/MAC을 사용한다.

## 최종 판단

Varve의 핵심 framing, checked-offset parsing, scalable batch append, disk-backed latest-key
조회, typed codec, mmap/zero-copy 방어 기반은 상당히 구현되어 있다. 이번 검증은
“전혀 쓸 수 없는 상태”를 뜻하지 않는다. 다만 확인된 hard-link 이중 writer와
self-test 데이터 손실은 공개 라이브러리에서 허용할 수 없고, matrix sidecar/fatal
CRC와 manual trait invariant도 안정화 전에 해결해야 한다.

따라서 현 시점 평가는 다음과 같다.

- **통제된 내부 프로젝트 사용:** 위 제한을 강제하면 가능.
- **일반 사용자 대상 public release:** 릴리스 차단 7개 수정 전 비권고.
- **PB-scale 성능 주장:** 주소·bounded-memory 설계와 1 TiB probe까지는 입증됨.
  실제 PiB와 장시간 통계 benchmark는 아직 입증되지 않음.
- **보안 주장:** safe memory-safety 결함은 발견되지 않았으나 증명된 것은 아님.
  데이터 무결성·가용성 결함은 위와 같이 실제 존재함.

## 외부 의미 기준

- [RustSec cargo-audit / cargo-deny 설명](https://rustsec.org/)
- [redb Durability 의미](https://docs.rs/redb/latest/redb/enum.Durability.html)
- [Windows process object lifetime](https://learn.microsoft.com/en-us/windows/win32/procthread/terminating-a-process)
- [WaitForSingleObject 계약](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject)
