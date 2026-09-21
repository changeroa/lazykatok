# lazykatok

로컬 우선(local-first) 카카오톡 터미널 클라이언트. 방 목록을 최신순으로 골라 들어가는
실시간 대화 TUI, 로컬 암호화 아카이브, 키워드·BM25·의미 검색을 하나의 CLI로 제공합니다.
Apple Silicon macOS 전용.

[![CI](https://github.com/changeroa/lazykatok/actions/workflows/ci.yml/badge.svg)](https://github.com/changeroa/lazykatok/actions/workflows/ci.yml)

## 특징

- **대화 TUI (`lazykatok chat`)** — 마지막 메시지가 최신인 방이 목록 맨 아래(입력
  프롬프트 바로 위)에 1번으로 오는 방 선택, 로컬 시각 `HH:MM` + 보낸 사람 열 + 연속
  메시지 `·` 묶음 + 날짜 구분선으로 정돈된 대화 뷰, readline식 입력 편집, 스크롤과
  새 메시지 표식. 폴링과 전송이 백그라운드에서 돌아 입력은 항상 반응형입니다.
- **답장 전송** — TUI에서 Enter로 카카오톡 앱에 실제 전송(macOS Accessibility 경로).
  전송 중에도 입력 가능하고, 동시 전송은 안전을 위해 하나로 직렬화됩니다.
- **로컬 검색** — 키워드 / BM25 / 임베딩 의미 검색. 대화 데이터는 전부 로컬
  SQLCipher 아카이브에 저장되고 네트워크로 나가지 않습니다.

## 요구 사항

- Apple Silicon macOS
- 실행 중인 카카오톡 macOS 앱 (실시간 읽기·전송)
- Rust 1.91+ (직접 빌드 시)
- 터미널에 **전체 디스크 접근 권한**(카톡 DB 읽기)과 **접근성 권한**(전송) —

  시스템 설정 > 개인정보 보호 및 보안에서 부여하고 `lazykatok doctor`로 확인하세요.

## 설치

```bash
git clone https://github.com/changeroa/lazykatok.git
cd lazykatok
cargo build --release
# 바이너리: target/release/lazykatok
```

편하게 쓰려면 PATH에 둔 래퍼 하나면 충분합니다:

```bash
cat > ~/.local/bin/lazykatok <<'EOF'
#!/usr/bin/env bash
exec "$(dirname "$(git -C ~/src/lazykatok rev-parse --git-dir 2>/dev/null)")/target/release/lazykatok" "$@"
EOF
# 실제 클론 경로에 맞게 조정한 뒤 chmod +x
```

## 빠른 시작

```bash
lazykatok doctor                 # 권한·설정 상태 점검
lazykatok sync                   # 로컬 카톡 DB → 아카이브 동기화
lazykatok chat                   # 방 선택 → 대화 TUI 진입 (기본)
lazykatok chat-bg                # 카톡 창을 띄우지 않고 이미 열린 방에만 전송
lazykatok keyword 회식           # 키워드 검색 (--limit 5 등 옵션 전달)
lazykatok bm25 회식              # BM25 검색
lazykatok semantic "언제 만나기로 했지"  # 의미 검색
```

원본 CLI 그대로 쓰는 경우:

```bash
lazykatok watch --select --reply --accept-use-policy --source macos
lazykatok search keyword <질의> --json
lazykatok transcript --chat <chat_id> --from 2026-08-01 --to 2026-08-31 --out export.md
```

## TUI 사용법

방 목록은 마지막 메시지 시각이 오래된 방부터 위에서 아래로 정렬되고 번호는 아래가
1번입니다 — 가장 최근에 활동한 방이 입력 프롬프트 바로 위에 1번으로 놓입니다.
시각을 알 수 없는 방은 맨 위에 이름 순으로 표시됩니다.

| 키 | 동작 |
|---|---|
| `Enter` | 현재 초안을 선택한 방으로 전송 (`/send 메시지`도 동일) |
| `←` `→` `Home` `End`, `Ctrl-A/B/E/F` | 초안 커서 이동 |
| `Backspace` / `Delete`, `Ctrl-W/U/K` | 글자·단어·행 삭제 |
| `↑` `↓` / `PgUp` `PgDn` | 대화 스크롤 / 페이지 |
| `Ctrl-C` 또는 `Ctrl-D` | 종료 |
| `/help`, `/quit` | 도움말, 종료 |

- 메시지는 `HH:MM 보낸사람 본문` 형태로, 같은 사람이 5분 안에 보낸 연속 메시지는
  `·`로 묶여 표시됩니다. 날짜가 바뀌면 날짜 구분선이 나타납니다. 색상 구분 없이
  기본 전경색으로 렌더링됩니다.
- 새 메시지가 도착해도 작성 중인 초안과 커서는 그대로 유지됩니다. 과거 기록을
  보는 중이면 화면은 흔들리지 않고 `N new messages` 표식만 표시됩니다.
- 전송은 즉시 `전송 중…` 상태 줄과 함께 백그라운드로 실행되고, 결과는
  `sent reply (N chars)` 또는 `send failed: ...` 줄로 알림됩니다. 전송 중 추가
  Enter는 초안을 지키고 이전 전송 완료를 기다립니다.
- paste는 텍스트 삽입일 뿐 절대 전송을 유발하지 않습니다. 붙여넣은 줄바꿈은
  공백으로 변환됩니다.

### 전송 주의사항

전송은 되돌릴 수 없고 상대에게 실제로 도착합니다. `--reply-no-open`(래퍼의
`chat-bg`)은 카톡 창을 띄우거나 포커스를 가져가지 않고 이미 열려 있는 방에만
조용히 전송합니다. 기본 경로는 필요할 때만 잠깐 커튼을 띄우고 카톡을 앞으로
가져옵니다. 자세한 정책은 [ACCEPTABLE_USE_POLICY.md](ACCEPTABLE_USE_POLICY.md)와
[DISCLAIMER.md](DISCLAIMER.md)를 참고하세요.

## 프라이버시

- 모든 대화 데이터는 로컬(`~/Library/Application Support/katok` — 이전 이름의
  디렉터리를 그대로 사용) SQLCipher 암호화 아카이브에 저장됩니다.
- 텔레메트리가 없고, 대화 내용이 네트워크로 전송되지 않습니다. 의미 검색의
  임베딩도 로컬 모델로 계산됩니다.
- 이 저장소는 공개입니다. 실제 대화에서 유래한 어떤 값도 커밋되지 않으며,
  테스트는 합성 fixture만 사용합니다 (`scripts/verify_release_config.py`의
  프라이버시 오라클이 이를 계속 검사합니다).

## 개발

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
python3 scripts/verify_release_config.py
```

## 라이선스

MIT. 이 프로젝트는 개인적으로만 사용 가능하고 사용 책임은 본인에게 있습니다. 타인에게 공유하거나 상업적 사용을 하지마세요.
