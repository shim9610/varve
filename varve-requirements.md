# varve_format! 요구사항 명세 — 작업 에이전트 핸드오프

> **이 문서의 독자:** varve(바이너리 포맷 정의 매크로 DSL 라이브러리)를 구현하는 작업 에이전트.
> 이 문서만으로 작업 가능하도록 자기완결형으로 작성됨. quark_binary 를 모른다고 가정한다.
>
> **목적:** 기존 도메인 바이너리 포맷(참조 구현 = `C:\git\PCR\crates\quark_binary`)의 기능을 varve 위에서 **보존**하기 위해 varve 가 갖춰야 할 요구사항을, 우선순위·의미·DSL 문법안·수용기준과 함께 명세한다.
>
> **주의:** 아래 DSL 문법은 **방향 제시용 스케치**다. 최종 문법은 varve 설계자가 정한다. 단 **의미(semantics)와 수용기준은 요구사항**이다.

---

## 0. 작업 범위 / 비목표

- **범위:** 아래 P0~P2 요구사항을 varve DSL + 생성 코드(reader/writer)에 반영.
- **비목표:**
  - quark_binary 코드를 그대로 복제하지 말 것. 패턴 참고만(참조 위치는 §5).
  - 도메인 로직(PCR 분석 알고리즘) 이식 아님 — 순수 저장 포맷 계층만.
  - 기존 .vrv 파일 호환은 요구하지 않음(신규 설계).

---

## 1. 배경 — 두 저장 패러다임 (가장 중요)

varve 의 현재 모델과 참조 포맷의 모델은 **근본적으로 다르다.** 이 차이를 이해하지 못하면 요구사항을 오해한다.

| 축 | varve (현재) | 참조 포맷 (호스팅 대상) |
|---|---|---|
| 저장 모델 | **append-only 레코드 로그** | **사전할당 행렬 + 슬롯 in-place 덮어쓰기** |
| 인덱스 | prev-offset 체인 / scan / checkpoint | offset table 직접 주소화 `idx = scan*n_ch + ch` |
| 유효성 | record footer / transaction marker (레코드 단위) | **commit 비트맵이 유일한 유효성 source** (셀 단위, 다중 카테고리) |
| 파일 크기 | 재작성 시 증가 | **유계(bounded)** — 최대 차원 zero-pad, in-place |
| 변경 | 추가만 가능 | 재분석이 같은 슬롯을 size-stable in-place 로 덮어씀 |

varve 는 **이벤트 로그/불변 스트림**에 강하고, 참조 포맷은 **차원이 런타임에 정해지는 가변·희소커밋 행렬 저장소**다. varve 가 후자를 호스팅하려면 아래 확장이 필요하다.

**append-only 데이터(예: 측정 원본 스트림)는 varve 현재 모델로도 잘 표현된다.** 확장이 필요한 건 (1) 사전할당 행렬, (2) 셀단위 commit 비트맵, (3) 그 위의 무결성·복구·durability 계층이다.

---

## 2. 용어

- **block**: 포맷이 저장하는 한 종류의 레코드/구조. varve 의 `fixed`/`variable` 에 더해 본 명세는 `matrix` 를 추가 요구.
- **matrix block**: 런타임 차원(예: `scan × ch`)으로 사전할당된 슬롯 테이블. 각 슬롯은 직접 주소화로 O(1) 접근.
- **cell**: matrix block 의 한 슬롯 = 한 키 조합(예: `(scan=5, ch=2)`).
- **commit bitmap**: 어떤 셀/레코드가 "유효(확정)"인지 나타내는 비트 배열. **레코드의 물리적 존재와 독립** — 비트가 0이면 데이터가 있어도 무효.
- **region CRC**: 헤더/메타/인덱스/commit map/각 레코드/각 섹션 등 **영역 단위** 체크섬.
- **fsync-then-emit**: 데이터→인덱스→commit 순으로 durable flush 한 뒤에만 상위 호스트가 "완료 이벤트"를 낼 수 있게 하는 순서 보장.
- **sidecar / companion**: 본체 파일과 짝을 이루는 보조 파일(예: 재분석 산출물).

---

## 3. 요구사항

각 항목: **REQ-ID · 요구 · 이유 · 의미 · DSL 스케치 · 수용기준(AC)**.

### P0 — 없으면 호스팅 불가 (핵심 패러다임)

---

#### REQ-1. 고정 행렬(matrix) 블록 + 직접 주소화
- **요구:** 런타임 차원으로 사전할당되는 matrix 블록. 복합 키 `(scan, ch)` 로 `offset = base + (scan*n_ch + ch) * stride` 직접 접근. prev-offset 체인 아님.
- **이유:** 분석 데이터는 `n_scans × n_channels` 행렬이며 워커가 임의 순서로 셀을 채운다. 체인 순회 없이 O(1) 임의접근이 필요.
- **의미:** 블록 생성 시 차원이 정해지면 슬롯 테이블(각 슬롯의 offset/size)이 예약된다. 슬롯은 비순차로 채워질 수 있다.
- **DSL 스케치:**
  ```
  matrix Analysis(dims = [scan, ch], id = 10) {
      tuned_centroids: [f32; 2] * max(n_wells),
      rfu:             f32       * max(n_wells),
      finetune_mask:   bitmap(max(n_wells)),
      inverse_meta:    InverseMeta,
  }
  ```
- **AC:**
  - `(scan, ch)` 임의 순서로 써도 각 셀을 O(1) 로 읽는다.
  - 슬롯 주소 계산에 다른 셀 데이터 의존 없음(체인 없음).
  - 차원은 create 시점 파라미터로 결정(REQ-10).

---

#### REQ-2. 셀 단위 commit 비트맵 = 유효성 단일 source
- **요구:** 레코드 존재와 **분리된** 별도 commit 비트맵이 유일한 유효성 신호. 비트가 0이면 reader 는 데이터가 있어도 `NotCommitted` 로 거부. 같은 논리 블록 내 **부분 commit**(일부 셀만 valid) 지원. 같은 키 공간 위에 **다중 독립 카테고리**.
- **이유:** 비정상 종료 시 "데이터는 기록됐지만 확정 안 됨" 상태를 안전하게 무시/복구해야 한다. 분석 단계가 여러 종류(원본/분석/마커/곡선/보정 등)라 각각 독립 유효성이 필요.
- **의미:** writer 가 데이터+인덱스를 durable 기록한 **뒤에야** 해당 commit 비트를 set. reader 는 commit 비트만 신뢰한다.
- **DSL 스케치:**
  ```
  commit: cell_bitmap {
      keyspace    = [scan, ch];                 // 2D 비트맵 (셀 단위)
      categories  = [raw, analysis, raw_markers, smooth_markers,
                     processed_curves, compensated];
      singles     = [master_grid, score];       // 단일 비트(전역)
      per_channel = [threshold];                 // 채널별 비트
  }
  ```
- **AC:**
  - 레코드 물리 존재 ≠ 유효. reader 는 commit 비트만으로 유효 판정.
  - 한 카테고리에서 일부 셀만 valid 한 상태가 표현·조회 가능.
  - 카테고리별로 독립적으로 set/clear.

---

#### REQ-3. in-place 덮어쓰기 + 유계(bounded) 파일 크기
- **요구:** 기존 슬롯을 **동일 크기 in-place** 로 덮어쓰기(재분석). 가변 길이 셀은 최대 차원(`max(n_wells)`)으로 zero-pad 하여 파일 크기를 결정적으로 유지.
- **이유:** 재분석을 반복해도 파일이 무한히 커지면 안 된다(타깃: 제한된 디스크/RAM). 결정적 크기·in-place 머지는 의도된 설계.
- **의미:** append-only 의 "새 레코드 추가 후 옛것 무시"가 아니라, 같은 물리 위치를 덮어쓴다. 크기가 바뀌면 거부(또는 재배치 정책 명시).
- **DSL 스케치:**
  ```
  storage: preallocated {
      overwrite = same_size_inplace;   // 동일 크기면 in-place, 아니면 오류
      pad_to    = max(n_wells);        // zero-pad 유계
  }
  ```
- **AC:**
  - 같은 셀 N회 재기록해도 파일 크기 불변.
  - 크기 불일치 덮어쓰기는 명확히 오류 처리.
  - (대안 허용: latest-wins append + 컴팩션. 단 **유계 보장**을 별도 충족해야 함.)

---

### P1 — 없으면 안정성/운영 기능 상실

---

#### REQ-4. 영역 단위 CRC + in-place 갱신 후 재계산
- **요구:** 헤더·메타·인덱스·commit map·각 레코드·각 섹션을 **각각** CRC32 로 보호. in-place 갱신 시 영향 영역의 CRC 만 재계산·재기록.
- **이유:** 가변 메타 영역(인덱스/commit map)이 반복 재기록되므로 레코드 단위 체크섬만으론 부족.
- **DSL 스케치:**
  ```
  integrity: crc32 {
      scope            = [header, meta, index, commit_map, per_record, per_section];
      recompute_on_inplace = true;
  }
  ```
- **AC:** 임의 영역 1바이트 변조 시 해당 영역 CRC 검증 실패로 검출. in-place 갱신 후 해당 영역 CRC 정합.

---

#### REQ-5. corruption 분류 + 표적 복구 액션
- **요구:** 손상을 **영역별로 분류**(치명 vs 복구가능)하고, "미커밋 tail 무시"를 넘는 복구 액션 제공: 셀 commit clear, 카테고리 전체 clear, **commit map 재구성(모든 entry CRC 재검증→비트맵 복원)**, 특정 지점 이후 truncate, resume.
- **이유:** 부분 손상에서 유효 데이터를 최대한 보존하며 안전 복구해야 한다.
- **의미:** open 시 손상 감지 → 분류 → 권고 액션 반환 → 호스트 결정 → 복구 실행.
- **DSL 스케치:**
  ```
  recovery {
      fatal   = [header_crc, magic_mismatch, raw_commit_map_crc];
      actions = [clear_cell, clear_category, rebuild_commit_map,
                 truncate_after, resume];
      rebuild = verify_each_entry_crc;   // entry CRC 재검증으로 비트맵 재구성
  }
  ```
- **AC:**
  - 손상 종류별로 recoverable 여부 + 권고 액션을 구조화 반환.
  - `rebuild_commit_map` 이 모든 entry CRC 를 재검증해 유효 비트만 set.
  - 치명 분류는 복구 불가로 명확히 처리.

---

#### REQ-6. fsync-then-emit 순서 배리어 + 커밋 후 훅
- **요구:** `데이터 sync_data → 인덱스 sync_data → commit map sync_all` 3단계 순서 강제. durable 완료 **후에만** 호스트가 진행 이벤트를 낼 수 있도록 post-commit 콜백/훅 노출.
- **이유:** 상위 시스템이 라이브로 진행상황을 스트리밍한다. "기록 안 끝났는데 완료 이벤트" 가 나가면 reader 와 불일치(데이터 무결성 사고).
- **DSL 스케치:**
  ```
  durability: ordered_barrier {
      phases          = [data: sync_data, index: sync_data, commit: sync_all];
      post_commit_hook = true;   // (block_id, key, seq) 콜백
  }
  ```
- **AC:**
  - write 메서드 반환 = commit 단계 sync 완료 이후.
  - post-commit 훅이 durable 시점 이후에 호출됨(테스트로 순서 검증).

---

#### REQ-7. resume 의미 + 동반(sidecar) 파일
- **요구:** 부분 진행 감지(예: M 중 N 커밋) → resume/restart/discard 신호. 본체 + 동반 파일(별 확장자) + reader 교차 dispatch + 본체 헤더의 "동반 활성" 플래그.
- **이유:** 장기 작업이 중단된 뒤 이어서/다시/버리기를 안전히 선택해야 하고, 재작업 산출물을 별 파일에 두고 나중에 병합한다.
- **DSL 스케치:**
  ```
  companion AnalArch(ext = "qana", parent_flag = reanalysis_active);
  resume: detect_partial { signals = [resume, restart, discard]; }
  ```
- **AC:** 비정상 종료 후 open 시 "부분 진행(N/M)" 을 감지해 신호 반환. 동반 파일 존재/부재·손상에 따라 분기.

---

### P2 — 표현력/성능

---

#### REQ-8. 풍부한 고정 payload 타입 + zero-copy view 접근
- **요구:** 필드 타입으로 고정배열(`[f32;2]`, `[f32;8]`, `[i32;2]`), **비트맵(LSB-first, 1bit/elem)**, 2D 행렬(`well × ch` 구조체), 가변 수치 스트림(`[u16]`/`[f32]`/`[i32]`) 지원. 그리고 **positional read 로 슬라이스를 반환하는 zero-copy view 접근 모드**(전체 역직렬화 금지).
- **이유:** 수만 well × 다채널 f32 텐서는 전체 구체화하면 메모리/속도 비용이 크다. `reader.users()` 식 전량 materialize 로는 부적합.
- **DSL 스케치:**
  ```
  access: view;   // getter + &[T] 슬라이스 반환 (pread, no full deserialize)
  ```
- **AC:** 대형 배열을 전체 역직렬화 없이 슬라이스로 읽음. 비트맵 1bit/elem 패킹.

---

#### REQ-9. 블록별 선택 압축 + 청크 단위 무결성
- **요구:** 전역 압축이 아니라 **블록별 opt-in**. 압축 블록은 고정 크기 청크(예: 1MB)로 나눠 zstd, **청크별 원본 CRC** 보관.
- **이유:** 측정 원본만 압축하고 분석 영역은 무압축(랜덤접근). 부분 손상 격리.
- **DSL 스케치:**
  ```
  block RawChannel { ... } compression = zstd(level = 3, chunk = 1MiB, chunk_crc = true);
  block Analysis { ... }   compression = none;
  ```
- **AC:** 블록 단위로 압축 on/off. 청크별 CRC 로 부분 검증.

---

#### REQ-10. 런타임 동적 차원을 1급 레이아웃 파라미터로
- **요구:** create 시점에 `n_scans / n_channels / max(n_wells)` 등 차원을 받아 모든 matrix 블록·비트맵 크기를 결정.
- **DSL 스케치:**
  ```
  let mut w = AppFormat::create("data.vrv", dims! { n_scans: 46, n_channels: 5, n_wells_max: 22000 })?;
  ```
- **AC:** 동일 포맷 선언이 서로 다른 차원으로 인스턴스화됨. 차원이 슬롯/비트맵 크기에 반영.

---

#### REQ-11. 선언적 버전 마이그레이션
- **요구:** 버전 간 필드/섹션 추가를 선언하면 마이그레이션 스캐폴딩(구버전 decode → 변환 → 미변경 데이터 byte-copy → swap) 생성.
- **이유:** 포맷 진화 시 수작업 마이그레이션은 오류 잦음. "유연한 업그레이드"의 핵심.
- **DSL 스케치:**
  ```
  migrate 3 -> 4 {
      add singles.threshold;
      add meta.per_channel_threshold: [Option<f32>; n_channels] = default;
      copy unchanged;
  }
  ```
- **AC:** v(N) 파일을 v(N+1) 로 무손실 변환. 미변경 섹션은 byte-copy.

---

#### REQ-12. 비커밋 보조 데이터
- **요구:** commit 비트가 없는 보조 데이터 블록(예: 행별 보조 인덱스) 개념.
- **AC:** 해당 블록은 commit 비트맵에 포함되지 않고 존재만으로 읽힘.

---

## 4. 채택 경로 / MVP 권고

- **MVP(분기점) = REQ-1 + REQ-2 + REQ-3.** 이 셋이 들어와야 사전할당-행렬-커밋 패러다임이 성립한다. 나머지는 그 위에 점증.
- **권장 순서:** P0(1·2·3) → P1(6 fsync-emit → 4 CRC → 5 recovery → 7 resume/sidecar) → P2.
- **하이브리드 단기 전략(참고):** append-only 스트림성 데이터는 **지금 varve 그대로** 흡수 가능. 행렬 분석 저장소는 REQ-1·2·3 완성 전까지 기존 참조 구현 유지. → varve 1차 목표를 REQ-1·2·3 으로 잡으면 즉시 합류 가능한 지점이 생긴다.

---

## 5. 참조 구현 매핑 (패턴 학습용 — 복제 금지)

`C:\git\PCR\crates\quark_binary` 에서 각 요구의 실제 구현 패턴을 볼 수 있다.

| 요구 | 참조 위치 |
|---|---|
| REQ-1 matrix/직접주소화 | `encode.rs:147-271`(offset table), `writer.rs:845-923`, `reader.rs:1389-1429`. idx 계산 `writer.rs:853-854` |
| REQ-2 commit 비트맵 SSOT | `types.rs:243-271`(CommitMap), `status.rs`, reader 거부 `reader.rs:274,353-357`, set `writer.rs:913-921` |
| REQ-3 in-place/유계 | `writer.rs:845-923`(in-place if exist), zero-pad `n_wells_max` `types.rs:79-83` |
| REQ-4 영역 CRC | `crc.rs`, `encode.rs:32-48,119-125,218-223,309-314`, 영역별 CRC |
| REQ-5 corruption/복구 | `corruption.rs:71-143`(분류 11종/recoverable), `writer.rs:561-740`(액션 6종, RebuildCommitMap `599-693`) |
| REQ-6 fsync-then-emit | `writer.rs:190-235`(9단계), `writer.rs:925-977` |
| REQ-7 resume/sidecar | `reader.rs:114-150`(.qana dispatch/플래그), `corruption.rs:114-125`(PendingResume) |
| REQ-8 타입/view | `traits.rs:1-131`(8 Encodable* trait, getter+slice) |
| REQ-9 블록별/청크 압축 | `compress.rs:1-85`(zstd 1MB chunk, 청크 CRC), `encode.rs:340-414`(RawTiffRecord) |
| REQ-10 동적 차원 | `types.rs:59-102`(SessionMeta), `types.rs:174-200`(OffsetTable len=N×C) |
| REQ-11 마이그레이션 | `migration_v3.rs:1-350`(v3→v4 절차적 — 이걸 선언적으로 끌어올리는 게 목표) |

---

## 6. 수용 검증(통합 시나리오)

작업 완료 판정용 end-to-end 시나리오:

1. 차원 `(scan=3, ch=2, well_max=100)` 로 create.
2. matrix 셀들을 **비순차**로 일부만 write + commit (예: (0,0),(2,1) 만).
3. reader: 커밋된 셀은 슬라이스로 zero-copy read, **미커밋 셀은 NotCommitted**.
4. 같은 셀을 동일 크기로 **in-place 재기록** → 파일 크기 불변 확인.
5. commit map 영역 1바이트 변조 후 open → corruption 분류 + `rebuild_commit_map` 으로 entry CRC 재검증→비트맵 복원.
6. write 도중 강제 중단 시뮬레이션 → 미커밋 tail 무시, 부분 진행(N/M) 신호.
7. v(N)→v(N+1) 마이그레이션 후 기존 셀 무손실.

위 7개가 통과하면 quark_binary 핵심 기능을 호스팅할 수 있다.
