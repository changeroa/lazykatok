# macOS 첫 설정

`katok`은 카카오톡 준비 상태를 자동으로 확인할 수 있지만, macOS는 CLI가 스스로 전체 디스크 접근 권한이나 손쉬운 사용 권한을 부여하는 것을 허용하지 않습니다. 설정 helper는 대신 아래 순서로 안내합니다.

1. `katok permissions macos`로 필요한 시스템 설정 화면을 엽니다.
2. 현재 터미널 앱을 허용하도록 안내합니다.
3. `katok doctor --macos-probe --json`으로 카카오톡 앱, 컨테이너, DB 접근 가능 여부를 확인합니다.
4. `katok sync --source macos --json`으로 로컬 아카이브를 만듭니다.
5. `katok index --json`으로 EmbeddingGemma 벡터 인덱스를 만듭니다.
6. semantic smoke search를 실행합니다.

helper 실행:

```bash
scripts/katok-macos-setup.sh
```

직접 빌드한 binary를 사용할 때:

```bash
KATOK_BIN=target/debug/katok scripts/katok-macos-setup.sh
```

`katok: command not found`가 나오면 먼저 설치와 PATH를 확인합니다.

```bash
cargo install katok
export PATH="$HOME/.cargo/bin:$PATH"
katok permissions macos --accessibility
```

권한 설정 화면만 직접 열려면:

```bash
katok permissions macos
```

KakaoTalk UI 자동화까지 쓸 계획이면 손쉬운 사용 설정도 같이 엽니다.

```bash
katok permissions macos --accessibility
```

## 완전 자동화가 안 되는 이유

Apple TCC 권한 시스템은 전체 디스크 접근 권한과 손쉬운 사용 권한에 대해 사용자의 시스템 설정 조작을 요구합니다. 로컬 앱은 설정 화면을 열고 결과를 확인할 수 있지만, 자기 자신에게 권한을 몰래 부여할 수는 없습니다.

## 기대 결과

권한을 켠 뒤 `katok doctor --macos-probe --json`은 대략 아래 상태를 보여야 합니다.

```json
{
  "source_adapter": {
    "macos": {
      "status": "checked",
      "app_installed": true,
      "container_present": true,
      "db_file_count": 1,
      "app_session": "store_open",
      "warning": null
    }
  }
}
```

정확한 `db_file_count`는 카카오톡 계정 상태와 로컬 동기화 상태에 따라 달라질 수 있습니다.

`app_session`은 실행 중인 카카오톡이 메시지 DB를 열고 있는지 보여 줍니다. `store_open`이면 정상입니다. `store_closed`이면 카카오톡이 로그인 창에 머물러 있는 등 로그아웃 상태일 가능성이 높습니다. 이때는 `sync`가 새 메시지를 가져오지 못하고 같은 경고를 stderr에 출력하니, 카카오톡에 다시 로그인하세요. `not_running`은 카카오톡이 꺼진 상태이고, `unknown`은 열린 파일 목록을 읽지 못한 경우입니다.
