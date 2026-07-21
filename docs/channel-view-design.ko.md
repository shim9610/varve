> 이 문서는 `docs/channel-view-design.md`의 한국어 번역본이다. 영문 원본과 내용이 어긋나는 경우에는 언제나 영문 원본이 정본이다.
> 모든 수치, 단위, `file:line` 인용은 원문 그대로이며 반올림하거나 바꾸지 않았다.

# Varve의 채널 선택적 접근 — 설계 (개정 2)

대상: Varve 0.4.0, 브랜치 `codex/dev-next`, 저장소 `C:\git\varve`.
상태: 설계 전용. 이 문서를 만들면서 저장소 파일은 하나도 수정하지 않았다.
모든 `file:line` 인용은 이번 개정에서 워킹 트리를 상대로 다시 검증했다. 이전 개정의 것을 믿고 그대로 가져오지 않았다.

개정 2는 적대적 리뷰를 반영한 결과다. 실질적으로 바뀐 절은
§0(전면 재작성), §2.2(신규 — 작은 윈도우에서의 교차점), §4.3(directory record를 예약된 내부 id 범위로 옮기고
commit 주기에 맞춤), §4.4(신규 — schema hash와 마이그레이션, 이전 개정은 틀렸다),
§4.7(신규 — 단일 record append 경로의 실제 syscall 산술), §5.5(동시성 명세화, cache 용도 변경),
§6(`read_extent_into` 추가, 두 예제 모두 재작성), §8(case 세 개 등급 하향),
§9.1, §9.2, §13(신규 — 마이그레이션), §12(위험도 재평가)다. 모든 변경에는 **[R2]** 표시를 붙였다.

---

## 0. 2분 요약 — 당신의 질문을, 당신의 용어로

**당신의 파일.** `datablock[CH1,CH2,CH3,CH4,CH1,…,CH4] metablock[…] datablock[…] datablock[…] metablock[…] …`,
이것이 끝없이 이어진다. datablock 하나가 record 하나다. 그 payload 안에는 네 채널 전부의 샘플이 interleaved로
잔뜩 들어 있다. metablock은 불규칙한 간격으로 사이사이에 끼어 있는 별개의 record다.

**파일 전체에서 CH1만 읽을 수 있는가?**

읽을 수 있다 — **단, payload 안에서 샘플을 interleaving하는 것을 그만둘 때만 그렇다.** 이것이 답의 전부다. 이건
*layout* 결정이지 index 결정이 아니며, 아무리 index를 잘 만들어도 layout을 대신할 수 없다.

- **payload가 당신이 그린 대로 interleaved로 남아 있다면:** CH1을 논리적으로 읽을 수는 있고, CH2–CH4의 디코딩을
  건너뛰므로 CPU를 약 4× 아낀다. 하지만 **디스크 I/O는 하나도 아끼지 못한다.** CH1의 바이트는 32바이트마다 반복된다.
  4 KiB 디스크 페이지에는 그 반복이 128번 들어 있다. 따라서 모든 datablock의 모든 페이지가 CH1이 필요로 하는
  페이지다. 파일의 25 %를 얻으려고 100 %를 읽는다. 이건 아무도 index로 고칠 수 없다.
- **payload를 planar로 쓴다면** — CH1의 4096개 샘플이 연속으로, 그다음 CH2, CH3, CH4가 차례로 놓이고, 맨 앞에
  각 채널이 어디서 시작하는지 알려주는 작은 고정 테이블을 둔다면 — CH1만 읽는 전체 스캔은 **1 TB 파일에서 1 TB가
  아니라 256 GiB만 읽는다.** datablock당 positional read 한 번이다. 이것이 진짜 1/C다.

**[R2] 그 1/C에 대한 정직한 단서.** 이건 *큰* 구간에 적용되는 이야기다. *작은* 윈도우 — 예를 들어 파일 중간
어딘가의 CH1 샘플 100개 — 에서는 planar와 interleaved가 구분되지 않는다. 100 samples × 4 ch × 8 B = 3200 B는
어느 쪽이든 한 페이지에 들어가기 때문이다. 교차점은 블록당 대략 `4096 / (C × W)` 샘플이다: **f64 4채널에서 128
샘플, 64채널에서 8샘플.** 그 아래에서는 layout이 아무 의미가 없고 비용을 지배하는 것은 *directory lookup*이다
(§2.2에서 800 B를 가져오는 데 ~356 KiB의 장치 트래픽이 든다는 것을 유도한다). 그 위에서는 planar가 완전한 1/C에
가까워진다. 정리하면: planar는 한 채널을 스캔할 때와 채널이 많을 때 결정적으로 옳고, 아주 작은 랜덤 조회에는
중립이다.

**쓰기 쪽에서 치르는 비용.**

| | 비용 |
|---|---|
| 샘플당 | 지금과 동일하다: 주소 계산 하나, store 하나. **할당 0회, syscall 0회.** writer가 열린 커서를 1개가 아니라 4개 들고 있을 뿐이다. |
| datablock당 | 이미 자기가 들고 있는 버퍼에 160 B 테이블(32 B header + 32 B × 4채널)을 쓰고, 이미 캐시에 뜨겁게 올라와 있는 바이트에 대해 CRC32를 4번 돌린다(32 MB/s에서 코어 하나의 ~4 %). **추가 syscall 0회, 추가 할당 0회.** |
| 버퍼링 / latency | **변화 없음.** datablock 하나가 이미 record 하나이고 record는 통째로 쓰이므로, 쓰기 전에 이미 128 KiB를 버퍼링하고 있다. planar는 그 버퍼 안에서 샘플이 어디에 놓이는지를 바꿀 뿐, 버퍼가 얼마나 큰지는 바꾸지 않는다. 채널당 1 MS/s에서 블록 latency는 4.096 ms 그대로다. |
| **[R2] directory flush당** | seek directory record는 **record마다가 아니라 sidecar commit 경계에서** 나간다: **commit당 syscall +2회**이지, datablock당이 아니다. 32 datablock마다 commit한다면 syscall +6 %이고, datablock 하나마다 commit한다면 **syscall이 3배**가 된다. 이 설계는 그 사실을 숨기지 않는다. §4.7. |
| 파일 크기 | commit당 32 datablock에서 **~0.19 %**, 1일 때 **~0.34 %** (§10.2). |

**[R2] 이전 개정이 틀렸던 것과 이번 개정이 고친 것.** 이전 개정은 directory에 대해 "추가 syscall 0회"라고
주장했지만, 그것이 배치 처리하는 `push_iter` 호출 안에서만 성립한다는 말을 하지 않았다. 또 schema hash가 그대로라고
주장하면서 동시에 새 등록 block type 두 개와 index-policy 플래그를 제안했는데, 셋 다 hash에 들어간다. 그리고 record
payload 안에 절대 파일 offset을 넣었는데, replacement 경로는 그것을 변환할 수 없다. 셋 다 아래에서 바로잡는다
(§4.7, §4.4, §4.3).

**당신만 내릴 수 있는 결정.** 설계를 막고 있는 것이 셋이다.

1. **writer가 planar payload를 내보내도 되는가?** 하드웨어가 당신이 손댈 수 없는 interleaved 버퍼를 DMA한다면,
   planar는 샘플당 load/store 쌍 하나(~1–2 ns)를 더 요구한다 — 실제로 한 번 더 훑는 것이고, 그래도 싸다. writer가
   샘플을 배치하는 쪽이라면 planar는 공짜다. 아래 모든 것이 여기에 달려 있다.
2. **부분 payload read를 record 전체 checksum 대신 extent별 CRC32로 검증해도 되는가?** 지금
   `read_payload_snapshot`(file.rs:494-508)은 payload 전체를 checksum한다. 부분 read는 구조적으로 그럴 수 없다.
   record 전체 checksum만이 유일한 무결성 수단으로 남아야 한다면, 채널 선택적 I/O는 불가능하고 디코딩 절감만 남는다.
3. **[R2] stream 파일을 처음부터 `block_offset_chain`을 켠 채로 만들어도 되는가?** 이 플래그를 켜면 계산되는
   schema hash가 달라지므로(format.rs:2396-2405가 `computed_schema_hash`에 들어가고, format.rs:1809), 이미 pin된
   기존 spec에 대해서는 transcode 없이 켤 수 없다. seek directory의 tail chain과 metablock만 빠르게 열거하는 데
   반드시 필요하다. §4.4와 §13.

나머지 넷은 범위를 막지는 않고 형태를 정하는 것이며 `openQuestions`에 있다.

---

## 1. 요구사항, 다시 진술

```
datablock[CH1, CH2, CH3, CH4, CH1, ..., CH4]  metablock[metadata]  datablock[...]  datablock[...]  metablock[...]  ...
```

소유자가 진술한 그대로의 사실:

- **datablock 하나가 record 하나다.** 채널마다 record가 하나씩 있는 것이 *아니다*.
- **그 record 하나의 payload 안에 CH1..CH4의 샘플이 여러 개 interleaved로 들어 있다.**
- **metablock record는 datablock 사이사이에 끼어 있고**, 간격은 불규칙하다.
- **stream은 무기한 이어진다.** 최종 길이를 알 수 없고, reader가 읽는 동안에도 파일이 자란다.

질문: *호출자가 파일 전체에 대해 CH1만, 또는 CH2만, 하는 식으로 질의할 수 있는가?* 그리고 라이브러리가 직접
해주지 않는다면, 최소한 사용자가 직접 구현할 수 있는 primitive는 노출해야 한다.

모든 비용 유도에 일관되게 적용되는 제약:

- **C1** 연속 고속 append가 최우선이다. record당 syscall 금지, record당 힙 할당 금지, append 경로에 연산당 O(N)
  작업 추가 금지.
- **C2** TB 규모. open은 header만 읽는다. 페이지는 필요할 때 fault로 들어온다. 메모리는 working set으로 제한되며
  전체 파일 내용에 비례하지 않는다. open 시 eager load는 허용되지 않는다.
- **C3** read는 `&self`이므로 핸들 하나로 여러 스레드를 동시에 서비스한다.

---

## 2. 물리 — 절대 물타기하면 안 되는 한 가지

**물리적으로 interleaved된 payload는 전체 블록 I/O보다 적게 읽으면서 채널 선택적으로 읽을 수 없다. 끝.**

`f64` 4채널: CH1의 바이트는 `C × W = 32 B` 주기로 반복된다. 스토리지 스택이 옮기는 최소 단위는 512 B 섹터이고
실제로는 4 KiB 페이지다. 4 KiB 페이지에는 `4096 / 32 = 128`개의 stride 주기가 들어가고 **그 하나하나에 CH1의
바이트가 들어 있다.** 모든 datablock의 모든 페이지가 CH1만 읽는 질의가 가져와야 하는 페이지다. CH1만 읽으면
datablock 바이트의 100 %를 전송하고 그중 25 %만 쓸모 있다.

index는 질문을 바이트 offset으로 옮기는 지도다. 장치가 바이트를 옮기는 단위를 바꿀 수는 없다. 따라서:

- index는 k번째 datablock을 *찾는 데* 필요한 O(k) 순회를 **없앤다**.
- index는 CH2..CH4의 *디코딩*을 **없앤다** — 이 형태에서는 실제로 CPU가 대략 4× 줄어든다.
- index는 CH2..CH4 바이트의 *read*는 **절대 없애지 못한다**.

**stride된 interleaved 저장에서 index가 I/O를 1/4로 줄여준다는 주장은 거짓이며, 이 설계는 그런 주장을 하지 않는다.**

같은 말이 matrix 서브시스템에도 대칭적으로 적용된다. matrix ordinal은

```rust
// crates/varve-core/src/matrix.rs:2146-2164
key.scan.checked_mul(dim1).and_then(|base| base.checked_add(key.ch))
```

이고 바이트 offset은 `slot_region_offset + ordinal * slot_stride`다(matrix.rs:2166-2172). `ch`가 가장 빨리
변하므로 matrix는 **scan-major**다. *scan* 하나는 연속이고, *channel* 하나는 `n_ch × slot_stride` 주기로
stride된다. `n_ch = 64, slot_stride = 8`이면 512바이트마다 쓸모 있는 바이트는 8개다. matrix는 정확히 당신이 하지
않는 질의에 최적화되어 있다.

### 2.1 지배적 질의별로 어떤 layout인가

| 지배적 질의 | 올바른 payload layout | 이유 |
|---|---|---|
| "어떤 시간 구간의 전 채널" | **interleaved** (그림대로) | 블록당 순차 read 한 번 |
| "긴 구간에 걸친 한 채널" | **planar** (채널별 연속 extent) | 바이트의 1/C, 블록당 채널당 read 한 번 |

interleaving에 *유리하게* 작용할 법한 경우 — datablock 하나의 모든 채널을 읽는 것 — 은 **planar에서도 손해가
아니다**(§8, case C6). 두 layout 모두 positional read 한 번으로 같은 payload 하나를 읽고, planar는 거기서 추가
I/O 없이 부분 슬라이스를 잘라낼 뿐이다. 이건 트레이드오프가 아니라 비대칭이고, 그래서 planar가 이긴다.

이 설계가 인정하는 2차적 단서: planar의 승리는 **SSD/NVMe**를 전제한다. seek이 7 ms인 회전 매체에서는 파일 전체에
걸쳐 128 KiB 블록마다 32 KiB read를 하는 것이 큐 깊이를 높여 interleaved로 스트리밍하는 것보다 느릴 수 있다. 이
설계는 NVMe/SSD를 대상으로 한다. 차가운 HDD 아카이브 계층은 계층별 권고가 따로 필요하다(`openQuestions`).

### 2.2 **[R2] 교차점 — planar가 더 이상 도움이 되지 않는 지점, 그리고 작은 질의를 실제로 지배하는 것

§0의 1/C 주장은 *전체 스캔*에 대한 주장이다. 이걸 평평한 성질로 읽으면 안 된다. datablock당 유도하면:

- 원하는 것: 이 블록에서 한 채널의 샘플 `n`개, 폭 `W`, 채널 수 `C`, 페이지 크기 `P = 4096`.
- **interleaved (L0):** 그 샘플들은 연속 `n × C × W` 바이트에 걸친다 → `ceil(n·C·W / P)` 페이지.
- **planar (L1):** 연속 `n × W` 바이트에 걸친다 → `ceil(n·W / P)` 페이지.

`n·C·W ≤ P`, 즉 `n ≤ P/(C·W)`인 동안에는 둘 다 **1페이지**다:

| C | W | 이 값 아래에서는 L0과 L1의 비용이 같아지는 블록당 샘플 수 |
|---|---|---|
| 4 | 8 (f64) | **128** |
| 8 | 8 | 64 |
| 64 | 8 | **8** |
| 4 | 2 (i16) | 512 |

그 임계값 위에서는 비율이 `C`를 향해 올라가고, `n·W ≫ P`가 되면 `C`에 도달한다. 전체 스캔 극단
(`n = 4096`, 블록 전체)에서 비율은 정확히 `C`다: **256 GiB 대 1 TB**, §6.1에서 검증한다.

**정리하면: 4채널에서 planar는 블록당 수백 샘플 이상 구간에서 의미가 있고 그 아래에서는 중립이다. 64채널에서는
거의 즉시 의미가 있다.** 권고는 어떤 채널 수에서도 유지되지만 *크기*는 그렇지 않고, §0이 이제 그렇게 말한다.

**작은 윈도우를 실제로 지배하는 것: layout이 아니라 directory다.** 권고 설계에서 "CH1 샘플 1 000 000..1 000 100"을
추적해 보자. 1 TB, 8.19×10⁶ datablock, directory group당 32 datablock, 262 144개 group. 샘플 1 000 000은
datablock 244(244 × 4096 = 999 424)의 블록 내 offset 576에 있고, 윈도우는 블록 경계를 걸치지 않는다.

| 단계 | read 횟수 | 논리 바이트 |
|---|---|---|
| tail에서 시작하는 CHK skip-list 하강, 차가운 상태 | ≤ 5 레벨 × ≤ 15 hop = **≤ 80** | ~13.4 KiB |
| BDR: header + CH1의 prefix 컬럼에 대한 이진 탐색 프로브 ~5회 + 엔트리 1개 | ~7 | ~90 B |
| 대상 datablock의 CET | 1 | 160 B |
| CH1 extent 자체 | 1 | 800 B |
| **합계** | **~89 positional read** | **~14.5 KiB** |

4 KiB 페이지 단위에서 그 ~89번의 read는 서로 다른 페이지 ~89개를 건드린다 ⇒ **800 B를 전달하려고 ~356 KiB의 장치
트래픽 — 445× 증폭이며, 전부 directory 탓이고 layout 탓은 하나도 없다.** 이전 개정은 read 횟수만 적어 놓고 비용이
작다고 독자가 추론하게 두었다. 차가운 작은 윈도우에서는 작지 않다.

정직한 결론 셋, 셋 다 이제 설계에 반영되어 있다:

1. **[R2] cache는 CET가 아니라 skip list에 붙어야 한다**(§5.5). 레벨 ≥ 1인 CHK를 전부 캐싱하면 1 TB에서
   `262144/16 = 16 384`개 노드 + 상위 레벨 ≈ 17 476 × 168 B ≈ **2.9 MiB**가 들고, 따뜻한 하강은 레벨 0 hop
   ≤ 15회로 줄어든다: read ~17회, 장치 트래픽 ~70 KiB, 87× 증폭. 그래도 여전히 증폭이다.
2. **append-only 가변 길이 파일에서 점 질의는 본질적으로 증폭된다.** 이 설계의 답은 하강 비용을 한 번만 치르고
   앞으로 걸어가는 범위 순회, 즉 `extents()`를 주된 API로 삼고, `read_into(start, ..)`는 하강 비용을 동반하는
   랜덤 액세스 편의 기능이라고 문서화하는 것이다.
3. 작고 흩어진 CH1 윈도우가 많이 필요하다면, 오름차순 `extents()` 한 번에 묶어라. 이 설계는 100샘플 랜덤 조회가
   싸다고 우기지 않는다.

---

## 3. 오늘 존재하는 것

### 3.1 주소 지정 개념은 이미 맞다, 구현이 틀렸을 뿐

```rust
// crates/varve-core/src/matrix.rs:834-838
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}
```

matrix는 이미 셀을 (scan, channel)로 주소 지정하고, channel은 이미 일급 *상태* 축이다.
`MatrixCommitKind::PerChannel`은 채널당 commit 비트를 하나씩 유지하고, `per_channel_count`(matrix.rs:5039-5051)는
말 그대로 `"ch" | "channel" | "channels" | "n_channels"`라는 이름의 차원을 골라서 그것을 해석하며, 없으면
`dimensions[1]`로 폴백한다. 공개 표면: `is_matrix_channel_committed` / `set_matrix_channel_committed`
(file.rs:4440, 1659, 2212).

그런데 조회 쪽 성질은 이 워크로드에 대해 전부 틀려 있다:

- **셀 하나씩만 된다.** `read_matrix_cell`(`VarveReader`의 file.rs:1632, writer의 2153, `VarveFile`의 4313)과
  `matrix_cell_payload`(file.rs:4318). *`matrix.rs` 어디에도 row도, column도, range도, iterator도 없다.* row는
  물리적으로 연속인데도 row API가 없다.
  **브리프 정정:** 브리프는 `read_matrix_cell`을 file.rs:1568로 인용하는데, 1568번 줄은 `VarveReader::path`다.
  실제 정의 세 개는 1632 / 2153 / 4313이다. 내용에는 영향이 없다.
- **`&mut self`인데**, 그 이유는 순전히 `&mut self.file`을 `matrix::read_cell`에 넘기기 때문이고, 그 함수는
  `file.seek(SeekFrom::Start(offset))?` 다음에 `file.read_exact(&mut payload)?`를 한다(matrix.rs:2747-2749).
  논리적으로 변경되는 것은 하나도 없다 — 바로 옆의 `matrix_cell_status`는 이미 `&self`다(file.rs:4356). **C3**
  위반이다.
- **open 시 bitmap을 eager하게 로드한다.** `open_layout`이 `load_commit_bitmaps`(matrix.rs:2518)와
  `load_crc_valid_bits`(matrix.rs:2527)를 호출한다. 그다음 `cell_status`(matrix.rs:2774)는 페이지를 전혀 fault
  시키지 않는 순수 인메모리 `SparseBitmap::get`이다. working set이 아니라 기록된 셀 수에 비례한다 — **C2** 위반.
- **차원이 create 시점에 고정된다.** `create_layout`(matrix.rs:2190)은 create 전용이고, open은 저장된 header를
  차원에서 다시 계산한 길이와 재검증한다(matrix.rs:2438-2453). `max_matrix_cells`는 기본 16 M(format.rs:114),
  `max_matrix_slot_region_len`은 8 GiB(format.rs:118)이고, matrix 데이터는 `append_log_start` *앞*에
  놓인다(matrix.rs:2278-2284). **matrix는 무기한 자라는 stream을 표현할 수 없다.**

개념은 있는데 구현이 틀린 곳에 있다. §9.10은 그래도 해둘 만한 matrix 수정 하나를 남겨둔다.

### 3.2 저장소에 이미 있는 관용적 답: 쓸 때 de-interleave하기

```
variable TdmsChannelChunk(id = 103, key = [group, channel, chunk_index]) {
    start_index, data_type, values_f64: Vec<f64> = default, values_i64: Vec<i64> = default
}
```
(tdms_model.rs:46-81; 224-280의 `write_first_tdms_segment`는 "Amplitude" 채널에 대해서만 chunk 하나를 쓴다.)

이건 *record 단위의 planar*다. 지금도 동작하고, 올바른 임시방편이며, 틀린 최종형이다(§4.5).

### 3.3 타입별 record 접근이 O(전체 record 수)다

```rust
// crates/varve-core/src/file.rs:3848-3853
pub fn blocks<T: VarveBlock>(&self) -> Result<BlockVec<T>> {
    crate::collections::ensure_registered_block::<T>(self.spec)?;
    let entries = clone_matching_entries(self.spec, &self.index, |entry| entry.block_id == T::ID)?;
    Ok(BlockVec::new(self.spec, self.snapshot.clone(), entries))
}
```

`clone_matching_entries`(file.rs:9063-9084)는 **선형 순회를 두 번 완전히** 한다 — 할당 크기를 잡으려고
`.filter().count()` 한 번, 채우려고 `.filter().cloned()` 한 번 — 그리고 `count × 104 B`를 복사한다.
`RecordIndexEntry`(file.rs:385-401)는 4 × u64 + 3 × `Option<u64>`(48 B, niche 없음) + 3 × u32 + 2 × u16 + bool
⇒ **104 B**이고, 이는 `index_bytes_for_count`(file.rs:8966-8979)가 청구하는 값과 정확히 같다.

이것이 동작하는 이유는 오로지 `self.index`가 존재하기 때문이고, 모든 `VarveFile::open*` 경로는 조건 없이
`load_index` → `scan_records_from`(file.rs:8383-8395, 8665-8771)을 호출한다. 따라서 **resident 경로에서
datablock과 metablock을 분리하는 데는 open 시 Θ(N), 호출당 2N번의 비교, 그리고 104·N의 resident 하한이 든다** —
record 8×10⁸개에서 83 GB다. **C2** 위반. `IndexPolicy.scan_on_open`(format.rs:477)은 존재하지만 open 경로가
참조하지 않는다.

### 3.4 저장되지만 아무도 읽지 않는 block별 chain

```rust
// crates/varve-core/src/format.rs:481-487
pub struct IndexPolicy {
    pub scan_on_open: bool,
    pub checkpoint_on_flush: bool,
    pub block_offset_chain: bool,
    pub keyed_offset_chain: bool,
}
```

`block_offset_chain`이 켜져 있으면 모든 record footer가 `prev_same_block_offset`을 담는다. footer 바이트 8에
있는 절대 `u64`이고, `0`은 "선행자 없음"을 뜻한다(`native_layout.rs:122-158`; `nonzero_offset`은 file.rs:8571).
이건 **block id별 역방향 연결 리스트가 이미 디스크에 있다**는 뜻이다. 쓰기 쪽은 `BlockTails`를 통해 이것을
유지한다(file.rs:5291-5307, 8285-8320, 8376). `BlockTails`는 이진 탐색 `tail()`을 가진 `Vec<(u32,u64)>`다
(file.rs:800-804, 836-841). 읽기 쪽은 이것을 `RecordIndexEntry.prev_same_block_offset`로 디코딩하고
(file.rs:7897, 8571-8596) layout 덤프에 출력한다 — **그리고 아무도 따라가지 않는다.**

명시해야 할 비용이 둘 있다:

- footer는 `commit_policy.requires_record_footer() || index_policy.requires_record_footer()`일 때만 쓰인다
  (`spec_needs_record_footer`, format.rs:1992-1994). 따라서 chain을 켜면 chain에 참여하는 block type뿐 아니라
  **모든 block type의 모든 record에 32 B가 붙는다.**
- **[R2] 편의용 preset이 원치 않는 플래그를 끌고 온다.** `IndexPolicy::BlockOffsetChain`(format.rs:509-514)은
  `{ scan_on_open: true, checkpoint_on_flush: false, block_offset_chain: true, keyed_offset_chain: false }`다.
  `scan_on_open: true`가 바로 §3.3이 설명하는 **C2** 위반이다. 이 설계가 원하는 구성은 명시적 생성자
  **`IndexPolicy::new(false, false, true, false)`**(format.rs:520-533)이지 preset이 아니다. "chain을 켜라"고만
  하고 "단, preset으로는 켜지 마라"를 빠뜨린 문서는 O(N) open 결함을 사고로 출하하게 된다.

### 3.5 올바른 `&self` primitive는 존재하는데 손이 닿지 않는다

```rust
// crates/varve-core/src/snapshot.rs:74
pub(crate) fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()>
```

검증함: `&self`이고, `SnapshotBounds`에 대해 `check_range`로 경계 검사를 하며, `read_at`을 반복 호출한다.
`read_at`은 unix에서 `FileExt::read_at`(pread), Windows에서 `FileExt::seek_read`다(snapshot.rs:253-261). 그리고
**호출자가 준 버퍼**에 쓴다. syscall 한 번, 할당 없음. `SnapshotFile { file: Arc<File>, bounds: SnapshotBounds }`
(snapshot.rs:15-18)는 필드 구성상 `Send + Sync`다. `with_len`은 파일을 다시 stat한다(snapshot.rs:42-48 →
`checked_snapshot_bounds`가 `file.metadata().len()`을 읽는다). 이 덕분에 O(1) tail-follow가 가능하다.

그런데 이건 `pub(crate)`이고 어느 크레이트 루트에서도 re-export되지 않는다. 모든 *공개* record read는 payload를
통째로 만들어낸다 — `read_payload`(file.rs:411), `read_logical_payload`(file.rs:445)가 그렇고, 그 뒤는
`read_payload_snapshot` → `read_vec_at`(file.rs:494-508)이다. **record 경로 어디에도
`(entry, byte_offset, byte_len) → bytes` API가 없다.** 공개 표면에서 offset+len read는
`read_matrix_aux(name, offset, len)`(file.rs:4328) 하나뿐이고, 그건 payload가 아니라 고정 aux 영역을 주소 지정한다.

### 3.6 C2와 C3을 이미 만족하는 유일한 서브시스템

`VarveStreamReader::open_native`(stream.rs:458-475)는 파일을 열고, `metadata().len()`을 읽고, 파일 header를 읽고,
`SnapshotFile`을 만든다. **스캔이 없다.** `VarveStreamReader::open`(stream.rs:439-456)은 여기에 더해 경로를
canonicalize하고, primary identity를 읽고, sidecar `DiskIndexStore`를 열고, snapshot을 시작하고,
`verify_primary_generation`을 호출하고, 한 줄로 가시성을 고정한다:

```rust
// crates/varve-core/src/stream.rs:450
reader.snapshot = reader.snapshot.with_len(snapshot.committed_eof())?;
```

`StreamingBlocks::next`는 32 B header만 읽고 남의 record를 건너뛴다:

```rust
// crates/varve-core/src/stream.rs:620-622
if entry.block_id != T::ID {
    continue;
}
```

**[R2] `events()`는 이미 존재하고 공개되어 있다**(stream.rs:516-522). 이 함수는 payload를 **전혀 읽지 않는**
header 전용 스캔에서
`BlockEvent { block_id, block_version, sequence, record_offset, payload_offset, payload_len }`(file.rs:565-572)를
내놓는다 — `StreamEvents::next`는 `scanner.next_entry()`를 호출하고 변환할 뿐 그 외에는 아무것도 하지
않는다(stream.rs:563-581). 이전 개정은 이걸 언급조차 하지 않았고 그 결과 P1과 case C8 둘 다 값을 잘못 매겼다.
§7과 §8에서 바로잡는다.

`VarveIndexedReader::lookup` / `get`은 redb read 트랜잭션 위의 `&self`다(indexed.rs:210, 225).

한계가 둘 있다. scanner는 언제나 `header_len`에서 시작하고(file.rs:7987-7988) — **공개 `scanner_at(offset)`이
없으므로 stream으로 랜덤 액세스로 진입할 방법이 없다** — 두 reader 모두 `feature = "high-cardinality-dev"` 뒤에
가려져 있으며 `default = []`다. 상수: `RECORD_HEADER_LEN = 32`, `RECORD_FOOTER_LEN = 32`(file.rs:41-42).

**[R2] open 비용, 검증되지 않은 절반까지 포함해서.** `open_native`는 O(header)임이 검증되었다:
`metadata().len()` + `read_file_header`, 스캔 없음. `open`은 거기에 `DiskIndexStore::open` +
`begin_snapshot_with_mode` + `verify_primary_generation`을 더한다(stream.rs:444-449). sidecar 크기는 O(B + K)일
법하다 — `advance_coverage_with_tail`(disk_index.rs:2275-2296)은 단조 frontier만 전진시키고 record별로는 아무것도
저장하지 않는다 — 그러나 redb 자체의 open 시점 작업과 sidecar 복구 패스는 **이 작업의 읽기 전용 제약 아래에서
측정되지 않았다.** 따라서 "open은 N에 대해 O(1)"이라는 주장은 **native 절반은 검증되었고 sidecar 절반은
단정일 뿐**이며, 요약에 사실로 적을 것이 아니라 벤치마크 목록에 올릴 항목이다.

### 3.7 이 설계가 조용히 물려받으면 안 되는 기존 결함 둘

- **`stage_state_records`가 chunk당 이차식이다.** stream.rs:1388-1395는 모든 record에 대해
  `records[index + 1..].iter().all(|(candidate, _)| candidate != block_id)`를 계산한다 — sidecar가 붙은 채
  `block_offset_chain`이 켜져 있으면 chunk당 O(chunk_len²)번의 정수 비교가 일어나고, 기본
  `max_records = 16_384`(stream.rs:138-159)에서 최대 ~2.7×10⁸번 비교다. 수정은 block id별 마지막 인덱스를
  계산하는 역방향 패스 한 번이면 된다. **chain에 무엇이든 얹기 전에 반드시 고쳐야 한다.**
- **batch append 경로가 record마다 할당한다.** `PreparedStreamRecord { bytes: Vec<u8>, .. }`
  (file.rs:8105-8113)는 record마다 힙에 할당되고 chunk 버퍼로 한 번 더 복사된다
  (`bytes.extend_from_slice(&record.bytes)`, stream.rs:1165). **제약 C1의 "record당 힙 할당 없음"은 여기서
  제안하는 것과 무관하게 오늘 이미 지켜지지 않고 있다.** 이 설계는 그 횟수에 아무것도 더하지 않고 아무것도 고치지
  않는다. §10.1의 record당 행이 이제 그렇게 명시한다.

### 3.8 Windows 커서 위험 (C3에 결정적)

Windows에서 `read_exact_at`은 `FileExt::seek_read`를 쓰는데(snapshot.rs:257-261), 이건 명시된 offset에서 읽으면서
*동시에* 핸들의 파일 포인터를 옮긴다. 그리고 `try_clone_file`(snapshot.rs:69-71)은 복제된 핸들을 넘겨주는데,
Windows에서는 **파일 포인터를 공유한다.** 그런 핸들 위에서 seek과 read를 두 개의 syscall로 하는 코드는 —
`matrix::read_cell`(matrix.rs:2747-2749), `read_payload_file_validated`(file.rs:8779-8784),
`read_stream_entry_at`(file.rs:8069), `NativeStreamScanner`(file.rs:7984-7992) — 다른 스레드의 positional read에
끼어들어 **조용히** 틀린 바이트를 반환할 수 있다.

**설계 규칙, 타협 불가:** 두 개 이상의 스레드에서 도달 가능한 핸들에서는 `read_exact_at`만 쓴다 —
`cursor_at` / `try_clone_file` + `seek`는 절대 금지. (`seek_read`의 정확한 커서 부작용은 이 작업의 읽기 전용 제약
아래에서 컴파일로 검증할 수 없었다. 확정된 사실이 아니라 검증해야 할 제약으로 취급하라.)

**[R2] 이 규칙에는 이전 개정이 부인했던 결과가 따라온다.** 이전 개정은 P3(`scanner_at`)를 "`read_stream_entry_at`이
이미 `pub(crate)`로 있으니 얇다"고 했다. 그 함수는 file.rs:8069에서 `snapshot.try_clone_file()?`을 하고 그다음
seek한다 — 정확히 금지된 패턴이다 — 그리고 `NativeStreamScanner`는 자기 자신의 seek하는 `file` 핸들을 들고 있다.
**P3는 얇지 않다. point read와 scanner를 먼저 `read_exact_at` 위로 옮기는 작업이 필요하다.** 그 비용은
C10(tail-follow)과 C13(N 스레드)로 전파되고, 둘 다 P3를 선행 조건으로 지목한다. §7과 §12를 바로잡았다.

---
