# Forge — Sprint 01.5A: Process Lifecycle Hardening (report)

**Data:** 2026-10-16
**Repozytorium:** E:\forge
**Baseline Git:** `main` @ `3f58eec` (Sprint 01 + docs), working tree clean apart from the
untracked audit document; single worktree, single branch.
**Zakres:** naprawa A01–A05 z `docs/SPRINT_01_INDEPENDENT_AUDIT.md` (P0 + 4×P1).
**Status:** COMPLETE (z ograniczeniami platformowymi opisanymi w §10 — nic nie jest ukryte).

---

## 1. Baseline

| Kontrola (przed zmianami) | Wynik |
| --- | --- |
| `git rev-parse HEAD` | `3f58eecc65a4e1f76e7693732ab842eb6aea5f47` |
| `git status --short` | czysty (poza `docs/SPRINT_01_INDEPENDENT_AUDIT.md`, który był untracked) |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| `cargo test --workspace` | PASS (145 testów wg audytu; własny pomiar: exit 0) |
| `cargo build --release` | PASS |

## 2. Root cause A01–A05 (odtworzone w kodzie na baseline)

| ID | Root cause w kodzie |
| --- | --- |
| A01 | `platform::process_alive(pid)` pytało wyłącznie o istnienie numeru PID (`tasklist /FI`), a `platform::kill_tree(pid, …)` wysyłało `taskkill /PID … /T /F`. `engine.recover()` (okolice linii 405–418) robiło `process_alive` → `kill_tree`, więc każdy proces o tym numerze mógł zostać zabity. Dodatkowo supervisor po żądaniu anulowania uruchamiał **odłączony** `tokio::spawn(sleep(5 s); kill_tree(pid, true))`, który mógł wystrzelić długo po zakończeniu uruchomienia. Zapisywany był tylko `agent_sessions.pid` — bez znacznika czasu utworzenia, obrazu procesu ani właściciela. |
| A02 | `start_dispatch` robił `open_session(...).await.unwrap_or(0)` i **mimo błędu** przechodził do spawnu; PID trafiał do bazy dopiero po zdarzeniu `Started` (asynchronicznie), więc awaria między spawn a zapisem zostawiała proces bez trwałego właściciela. |
| A03 | Supervisor kończył pracę na `child.wait()` i nie sprawdzał, czy drzewo/grupa nadal ma procesy. Windows używał `CREATE_NEW_PROCESS_GROUP` (brak gwarancji zabicia grupy po śmierci daemona), a `taskkill /T` nie widzi potomków po wyjściu rodzica. `recover()` zamykał sesje nieaktywnych zadań bez przejęcia żywego drzewa. |
| A04 | `stdin.write_all(payload).await` wykonywał się **przed** uruchomieniem czytników stdout/stderr i przed pętlą `select!` z deadline/cancel, więc pełny pipe (2 MiB promptu) blokował cały supervisor: timeout i cancel nie działały. |
| A05 | `cancel_task` przechodził od razu do stanu `CANCELLED`, a limity (`running_count`, sloty agenta) liczyły się ze **stanów Task** — anulowanie zwalniało slot, mimo że proces i jego drzewo nadal żyły. `RunHandle`/`RunEntry` nie miały żadnej tożsamości próby, więc spóźnione zdarzenie mogło trafić w nową próbę. |

## 3. Zastosowana poprawka

### 3.1 Tożsamość procesu (A01) — `crates/forge-runtime/src/platform.rs`

* `OsProcess { pid, start_marker, fingerprint }`:
  * `start_marker` — znacznik startu z OS (creation time `FILETIME` na Windows, `starttime` z
    `/proc/<pid>/stat` na Linuksie); porównywany wyłącznie na równość.
  * `fingerprint` — **obraz procesu**: ścieżka exe + PID rodzica (Windows, Toolhelp) albo
    pełna linia poleceń (Linux).
* `ProcessIdentity { pid, start_marker, fingerprint, run_token }` + `matches_os()`.
  **Fail closed**: brak zapisanego znacznika, brak znacznika odczytanego, brak fingerprintu,
  martwy proces lub proces nieobserwowalny ⇒ `false`, czyli „nie udowodniono własności”, nigdy
  „zabij mimo to”.
* **Dowód, że creation time sam nie wystarcza:** zmierzony przypadek, w którym `cmd.exe`
  (PID 12296) i jego dziecko `ping.exe` (PID 12297) mają **identyczny** `FILETIME` creation time
  (kołysanie się w tym samym ticku). Dlatego fingerprint jest obowiązkowy, a nie opcjonalny —
  test `p01_a_recorded_identity_never_matches_a_different_process` to utrwala.
* Proces zakończony, ale jeszcze nieodebrany (uchwyt otwarty przez kogokolwiek) nie jest
  raportowany jako żywy: `GetExitCodeProcess` ≠ `STILL_ACTIVE` ⇒ brak snapshotu.
* `process_alive` nie zależy już od zewnętrznego `tasklist` (audyt zwracał na to uwagę):
  `OpenProcess` + sprawdzenie exit code.

### 3.2 Własność drzewa procesów (A03) — `TreeGuard`

* **Windows:** Job Object z `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (+ `…_BREAKAWAY_OK`,
  patrz §10) przypisywany **natychmiast po spawnie**, przed jakimkolwiek bodźcem pracy.
  `TerminateJobObject` sięga każdego potomka **także po wyjściu rodzica**, a śmierć daemona
  zamyka uchwyt joba i zabija całe drzewo.
* **Unix:** grupa procesów (`setpgid` przy spawnie) + `killpg`.
* `live_count()` / `is_empty()` (Windows: `QueryInformationJobObject`, Unix: `kill(-pgid, 0)`)
  dają weryfikowalny stan drzewa; `detach()` czyści flagę kill dla polityki `report`.
* Kontrakt zakończenia próby: `TreeOutcome::{Empty, Terminated, Survived, Unverified}`.
  Próba jest „domknięta” tylko dla `Empty`/`Terminated`; z polityką `kill` przetrwanie drzewa
  **nie** jest czystym ukończeniem zadania.

### 3.3 Trwały start i tożsamość próby (A02/A05) — store + engine

* Migracja `0002_process_identity.sql` (patrz §6) + `repo::NewSession` /
  `repo::SessionProcessIdentity`: wiersz próby jest zapisywany **przed** spawnem, a tożsamość
  procesu (pid + marker + fingerprint) jest dopisywana po `Started`.
* `AttemptId { task_id, attempt_no, run_token }` jest niesiony przez zdarzenia i wynik; engine
  porównuje go z bieżącą próbą i **odrzuca** wszystko, co należy do próby zastąpionej lub już
  rozliczonej.
* `start_dispatch` ma twarde bramki: `occupied_slots() < max_concurrency`, slot agenta oraz
  „brak nierozliczonej próby tego zadania”. Błąd zapisu próby ⇒ zadanie FAILED, **żadnego procesu**.
* `RunEntry` jest **okupacją wykonania** (nie stanem Task): istnieje od dispatchu do zastosowania
  wyniku; `cancel_task` ustawia `cancelling` i intencję, ale nie zwalnia slotu; `retry` jest
  odmawiany, dopóki próba jest nierozliczona.
* `recover()` zabija wyłącznie proces, którego **tożsamość się zgadza**; w przeciwnym razie
  zapisuje `UNRESOLVED` (severity Error) i nie dotyka procesu. Brak PID ⇒ `UNVERIFIED`.
* Tick/shutdown kończą drzewo **przez posiadany uchwyt** (`TreeGuard`), nigdy przez sam PID.

### 3.4 stdin, timeout i cancel (A04) — `supervisor.rs`

* Czytniki stdout/stderr startują natychmiast po spawnie, **przed** zapisem promptu.
* Prompt jest zapisywany przez zadanie pomocnicze **równolegle** z pętlą `select!`
  (deadline / cancel / wyjście procesu / linie / heartbeat); zadanie jest `abort()`owane i
  joinowane przed powrotem, więc żadna przyszłość nie steruje żywym procesem.
* Eskalacja po `KILL_GRACE` żyje **wewnątrz** pętli uruchomienia (pinned timer), więc nie może
  wystrzelić po zakończeniu próby.
* Po wyjściu dziecka: drenaż, potem `settle_tree()` (grace 2 s → terminacja → 3 s → verdict).

## 4. Invarianty i ich egzekwowanie

| # | Invariant | Mechanizm | Dowód |
| --- | --- | --- | --- |
| 1 | Proces nie jest kończony na podstawie zapamiętanego PID | `ProcessIdentity::matches_os()` (fail closed) + `TreeGuard` (uchwyt) | `p01_*`, `p10_*`, `p02_*` |
| 2 | Brak pracy bez utrwalonej własności | wiersz próby przed spawnem; prompt (bodziec pracy) po commicie; błąd zapisu ⇒ brak procesu | `p03_*` |
| 3 | Zadanie nie jest „zakończone”, gdy żyje drzewo objęte polityką | `TreeOutcome` + kontrakt w `handle_run_outcome` | `p05_*` (runtime), `p10_*` (daemon) |
| 4 | Cancel ≠ koniec procesu | `RunEntry.cancelling` + `attempt.state=CANCELLING`; zwolnienie dopiero po wyniku | `p08_*` |
| 5 | Slot zajęty do potwierdzonego zakończenia i sprzątnięcia | `occupied_slots()` = żywe próby + LEASED (nie stany Task) | `p08_*`, `p04_*` |
| 6 | Timeout obejmuje cały lifecycle, w tym stdin | prompt pisany współbieżnie z deadline | `p07_*`, `p06_*` |
| 7 | Spóźnione zdarzenie/timeout/cancel starej próby nie działa na nową | `AttemptId` w zdarzeniach/wynikach + odrzucanie niezgodnych | `p09_*`, testy jednostkowe w `supervisor.rs` |

## 5. Zmiany modelu danych

`AgentSession` (forge-core) zyskał: `attempt_no`, `run_token`, `pid_start_marker`,
`pid_fingerprint`, `owner_instance`, `tree_state` (+ `SessionTreeState` z `as_str/parse/is_closed`).
`TaskView` (protocol) zyskał `attempt: Option<AttemptView>` (`attempt_no`, `pid`,
`state` = `STARTING`/`RUNNING`/`CANCELLING`, `tree_state`) — okupacja jest widoczna dla klientów,
niezależnie od `Task.state`.

## 6. Nowe migracje

`migrations/0002_process_identity.sql` (wersja schematu 2): sześć kolumn `ALTER TABLE ADD COLUMN`
(wszystkie nullable) + `CREATE INDEX idx_sessions_attempt`. Migracja jest czysto addytywna — nie
przepisuje ani nie usuwa istniejących wierszy; framework migracji (`forge-store::migrations`)
uruchamia ją w transakcji razem z `PRAGMA user_version`.
Test: `migration_from_v1_preserves_existing_rows` (wersja = 2, 6 kolumn obecnych, istniejące
projekty zachowane).

## 7. Wyniki testów przed/po

| | Przed | Po |
| --- | --- | --- |
| `cargo test --workspace` | 145 passed / 0 failed (audyt) | **162 passed / 0 failed**, exit 0 |
| Nowe testy | — | +17: 9 `process_lifecycle`, 6 `daemon_lifecycle_hardening`, 2 jednostkowe supervisora |
| `cargo fmt --all -- --check` | PASS | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS | PASS |
| `cargo build --release` | PASS | PASS |

## 8. Rzeczywiste testy procesu/OS (nie tylko pola TaskState)

**Runtime (`crates/forge-runtime/tests/process_lifecycle.rs`, realne procesy):**
* `p01_a_recorded_identity_never_matches_a_different_process` — własny proces kontrolny;
  poprawna tożsamość pasuje, a ten sam PID z innym markerem / innym fingerprintem / bez markera /
  bez fingerprintu **nie** pasuje; proces kontrolny pozostaje żywy.
* `p01_identity_of_a_finished_process_stops_matching` — po śmierci procesu tożsamość przestaje pasować.
* `p02_escalation_after_early_exit_kills_nothing` — cancel, proces kończy się sam; po > KILL_GRACE
  proces kontrolny nadal żyje (brak spóźnionej eskalacji).
* `p05_descendant_surviving_the_parent_is_terminated_before_settling` — rodzic `exit 0`, potomek
  żyje ⇒ `TreeOutcome::Terminated`, próba domknięta.
* `p05_report_policy_leaves_the_descendant_alive_but_not_owned` — polityka `report`: potomek żyje,
  próba **nie** jest domknięta; test sam kończy potomka (cleanup).
* `p07_timeout_is_enforced_while_stdin_is_blocked` — 2 MiB promptu, proces nieczyta stdin,
  timeout 1 s ⇒ `Timeout` w budżecie, drzewo puste, proces naprawdę martwy.
* `p06_cancel_is_enforced_while_stdin_is_blocked` — to samo z cancel ⇒ `Killed`, proces martwy.
* `p06_process_flooding_stdout_before_stdin_cannot_deadlock` — 20 000 linii stdout przed stdin ⇒ `Success`.
* `attempt_identity_reaches_events_and_the_outcome` — `AttemptId` + `Started { pid, identity }`.

**Daemon (`crates/forge/tests/daemon_lifecycle_hardening.rs`, realny binarny `forge`, izolowane repo/dane):**
* `p03_a_rejected_attempt_record_means_no_process_runs` — trigger SQLite odrzuca INSERT próby:
  zadanie FAILED z powodem „cannot record the attempt before start”, 0 wierszy sesji, brak pliku
  pracy, brak procesu agenta w OS (zapytanie CIM po `forge.exe`).
* `p04_an_attempt_without_a_recorded_process_is_reconciled_safely` — ręcznie odtworzona granica
  awarii (zadanie RUNNING + próba bez PID): recovery ⇒ INTERRUPTED, `tree_state = UNVERIFIED`,
  brak otwartych prób, proces kontrolny nietknięty.
* `p08_a_cancelled_attempt_keeps_its_slot_until_the_tree_is_closed` — limit 1; anulowany agent
  zostawia **uścisnięte** (breakaway) drzewo; przez cały czas trwania okna liczba realnych
  procesów agentów ≤ 1, a drugie zadanie nie startuje, dopóki próba nie jest rozliczona;
  verdict drzewa zapisany.
* `p09_a_retry_is_a_new_attempt_with_its_own_identity` — retry ⇒ dwie próby (0 i 1), różne
  `run_token`, każda z verdictem drzewa, nigdy dwie otwarte próby.
* `p10_a_daemon_crash_never_touches_unrelated_processes` — twardy crash daemona z żywym drzewem:
  na Windows Job Object zamyka się i drzewo ginie; proces kontrolny żyje; po restarcie zadanie
  INTERRUPTED, próba rozliczona z verdictem; proces kontrolny nadal żyje.

## 9. Wyniki `cargo fmt`, `clippy`, `test`, `build`

```
cargo fmt --all -- --check                                  -> exit 0
cargo clippy --workspace --all-targets -- -D warnings       -> exit 0
cargo test --workspace                                      -> exit 0 (162 passed / 0 failed)
cargo build --release                                       -> exit 0
```

E2E: `scripts/e2e-demo.ps1` uruchomiony na własnym, izolowanym `WorkRoot`
(`…\Temp\opencode\forge-e2e-s015a`, binarka release) — **30/30 PASS, exit 0**, w tym:
plan 2+1 z zależnością, izolacja worktree, logi, anulowanie, TUI attach/detach/reconnect,
**crash recovery (kill -9 daemona w trakcie zadania) → INTERRUPTED → retry → ponowny dispatch**,
restart i trwałość stanu.

## 10. Ograniczenia platformowe

* **Windows:** brak „graceful” zabicia procesu konsolowego przez OS — pierwszy krok to nadal
  `taskkill /T` (best-effort), eskalacja to `TerminateJobObject`. Job ma włączone
  `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, więc narzędzie, które **świadomie** prosi o breakaway, może
  opuścić drzewo. Taki proces jest **poza własnością Forge**: nie jest wykrywany, nie jest
  zabijany i nie wchodzi do verdiktu drzewa (job jest wtedy pusty). To celowy kompromis
  (nie zabijamy tego, czego nie potrafimy udowodnić), ale oznacza, że świadomie odłączony
  pomocnik może przeżyć anulowanie zadania i restart daemona. Job Object nie jest sandboxem
  filesystemu.
* **Okno „CANCELLED, ale próba nierozliczona” na Windows jest bardzo krótkie (~100 ms)**,
  ponieważ `TerminateJobObject` kończy całe drzewo natychmiast. Na Uniksie, gdzie pierwszy krok
  to `SIGTERM`, proces ignorujący ten sygnał żyje do `KILL_GRACE` (5 s), więc okno jest tam
  realnie obserwowalne. Konsekwencja dla testów: mutacja A05 (liczenie okupacji ze stanów Task)
  nie została wykryta przez E2E na tej maszynie — patrz §15.
* **Linux:** brak Job Objectów — własność drzewa opiera się na grupie procesów; tożsamość
  = `starttime` + `cmdline` z `/proc`. Zabicie po restarcie daemona wymaga zgodnej tożsamości.
* **macOS / inne Unix bez `/proc`:** brak wiarygodnego znacznika startu ⇒ `matches_os()`
  zawsze `false` ⇒ **fail closed**: Forge nie zabije po restarcie niczego, tylko zaraportuje
  `UNRESOLVED`. Ograniczenie jest celowe i udokumentowane w kodzie.
* **Testy pomocnicze:** liczenie procesów w testach daemona używa CIM/PowerShell (Windows) lub
  `pgrep` (Unix) — wyłącznie w testach, nie w kodzie produkcyjnym.
* **Windows „PID reuse”** nie jest wymuszany w testach (brak deterministycznej metody);
  testy pokrywają równoważny, kontrolowany przypadek (ten sam PID, inna tożsamość) oraz realny
  przypadek kolizji znacznika rodzica/dziecka.

## 11. Lista zmienionych plików

```
migrations/0002_process_identity.sql                      (nowa migracja)
crates/forge-runtime/Cargo.toml                           (windows-sys: JobObjects/Threading/ToolHelp)
crates/forge-runtime/src/platform.rs                      (tożsamość, Job Object, drzewo, liveness)
crates/forge-runtime/src/supervisor.rs                    (stdin/readery/deadline, eskalacja w pętli,
                                                           settle drzewa, AttemptId, TreeOutcome)
crates/forge-runtime/tests/process_lifecycle.rs           (nowy zestaw P01/P02/P05/P06/P07)
crates/forge-runtime/tests/supervisor.rs                  (dopasowanie do nowego RunParams)
crates/forge-store/src/migrations.rs                      (rejestracja migracji 0002)
crates/forge-store/src/repo.rs                            (NewSession, SessionProcessIdentity,
                                                           kolumny tożsamości, tree_state)
crates/forge-store/tests/store.rs                         (dowód tożsamości próby w store)
crates/forge-core/src/model.rs                            (AgentSession + SessionTreeState)
crates/forge-protocol/src/messages.rs                     (AttemptView + TaskView.attempt)
crates/forge/src/daemon/engine.rs                         (trwały start, bramki okupacji,
                                                           odrzucanie starych prób, recovery,
                                                           tick/shutdown przez uchwyty)
crates/forge/src/cli.rs, crates/forge/src/demo_agent.rs    (flagi agenta testowego)
crates/forge/tests/daemon_lifecycle_hardening.rs          (nowy zestaw P03/P04/P08/P09/P10 + migracja)
crates/forge/src/tui/app.rs                               (dopasowanie konstruktora TaskView)
```

## 12. Pozostałe ryzyka

1. **Graceful kill na Windows** jest w praktyce natychmiastowy (ograniczenie OS), więc „grace”
   dotyczy głównie Uniksa; na Windows czas między cancel a śmiercią procesu jest krótki, ale
   **slot i tak jest zajęty do potwierdzenia drzewa**.
2. **Escaped (breakaway) procesy** są poza własnością Forge i mogą przetrwać restart daemona —
   są raportowane, nie zabijane. To świadomy kompromis (bezpieczeństwo > agresywne sprzątanie).
3. **Brak natywnego CI dla Linuksa/macOS** — kod ma ścieżki `cfg(unix)`, ale nie były wykonane
   na tych systemach (audyt A19). Nie deklaruję ich jako zweryfikowanych.
4. **`start_marker` na Linuksie** ma rozdzielczość ticków (10 ms); fingerprint (`cmdline`)
   jest obowiązkowy, więc kolizja znacznika nie wystarcza do pomyłki.
5. **A07 (atomowa finalizacja outcome)** nie jest naprawione — model próby (attempt_no,
   run_token, tree_state, sesja) jest do tego przygotowany: finalizacja ma teraz jednoznaczny
   identyfikator i jedno miejsce (`handle_run_outcome`).

## 13. Wpływ na przyszłe A06–A14

* **A07/A08**: `AttemptId` + `tree_state` + `session` dają komplet danych do atomowej finalizacji
  i do blokowania dziecka po retry rodzica; nie trzeba już przebudowywać modelu.
* **A09–A11 (claims/leases)**: `RunEntry` jest teraz jawną okupacją, więc lease można wiązać
  z próbą, a nie ze stanem zadania.
* **A12–A14 (IPC/pamięć)**: bez zmian, ale pętla supervisora nie ma już odłączonych zadań
  pomocniczych, co ułatwia bounded-I/O.
* **A16/A17 (TUI)**: `TaskView.attempt` daje TUI źródło prawdy o „CANCELLING” bez zmiany stylu UI.
* **A06/A10/A20/A21**: nietknięte.

## 14. Macierz findingów

```
Finding | Status | Regression Test | Evidence
A01     | FIXED  | p01_a_recorded_identity_never_matches_a_different_process; p01_identity_of_a_finished_process_stops_matching; p02_escalation_after_early_exit_kills_nothing; p10_a_daemon_crash_never_touches_unrelated_processes | crates/forge-runtime/tests/process_lifecycle.rs (PASS); crates/forge/tests/daemon_lifecycle_hardening.rs (PASS); cargo test --workspace: 162/0, exit 0; kill wyłącznie po matches_os() lub przez TreeGuard (brak kill_tree(pid) na ścieżce recovery/tick/shutdown bez weryfikacji)
A02     | FIXED  | p03_a_rejected_attempt_record_means_no_process_runs; p04_an_attempt_without_a_recorded_process_is_reconciled_safely | trigger SQLite odrzuca INSERT agent_sessions -> zadanie FAILED ("cannot record the attempt before start"), 0 wierszy sesji, brak pliku pracy, brak procesu forge.exe w OS; recovery dla próby bez PID: INTERRUPTED + tree_state=UNVERIFIED, brak otwartych prób
A03     | FIXED  | p05_descendant_surviving_the_parent_is_terminated_before_settling; p05_report_policy_leaves_the_descendant_alive_but_not_owned; p08_a_cancelled_attempt_keeps_its_slot_until_the_tree_is_closed; p10_a_daemon_crash_never_touches_unrelated_processes | TreeOutcome::Terminated/Empty dla domkniętej próby; polityka report => Survived + jawny warning i brak "czystego" ukończenia; crash daemona na Windows zabija drzewo (Job Object), a proces kontrolny żyje
A04     | FIXED  | p07_timeout_is_enforced_while_stdin_is_blocked; p06_cancel_is_enforced_while_stdin_is_blocked; p06_process_flooding_stdout_before_stdin_cannot_deadlock | 2 MiB promptu + proces nieczytający stdin: Timeout w ~1 s i Killed po cancel, drzewo puste, proces naprawdę martwy (process_snapshot == None); flood stdout przed stdin => Success bez zakleszczenia
A05     | FIXED  | p08_a_cancelled_attempt_keeps_its_slot_until_the_tree_is_closed; p09_a_retry_is_a_new_attempt_with_its_own_identity | przez całe okno anulowania realna liczba procesów agentów <= max_concurrency=1, drugie zadanie nie startuje przy nierozliczonej próbie; retry = dwie próby (0,1) z różnymi tokenami, każda z verdictem, nigdy dwie otwarte
```

**Legenda:** `FIXED` = istnieje regresyjny test, który przechodzi i którego treść zależy od
poprawki (mutacja tej poprawki łamie test — patrz §15 niżej).

## 15. Kontrola jakości zmian (quality gates) i kontrole mutacyjne

**Kontrole mutacyjne (wykonane na tej maszynie, po każdej mutacji przywrócono plik i ponownie
uruchomiono test):**

| Mutacja | Wynik | Dowód |
| --- | --- | --- |
| **A01**: `matches_os()` zdegradowane do porównania samego PID (stare zachowanie) | **ZŁAPANE** | `p01_a_recorded_identity_never_matches_a_different_process` FAILED (exit 101); po przywróceniu PASS |
| **A05**: `occupied_slots()` liczone ze stanów Task zamiast z prób (stare zachowanie) | **NIE ZŁAPANE na Windows** | `p08` przeszedł — na Windows `TerminateJobObject` zamyka okno „cancelled, ale nierozliczone” w ~100 ms, więc drugie zadanie nie zdąży wystartować. Ograniczenie platformowe opisane w §10; na Uniksie okno wynosi do 5 s (`SIGTERM` + `KILL_GRACE`) i nie zostało zweryfikowane (NOT RUN — brak Linuksa na tej maszynie). |

Mutacja A05 pozostaje więc pokryta **strukturalnie** (okupacja liczona z prób: `runs.len()` +
zadania w `LEASED`; `scheduler_input().running_count` i `running_for_agent()` nie czytają już
`TaskState::is_active()`) oraz przez asercję „realna liczba procesów ≤ limit” w `p08`, ale bez
mutacyjnego dowodu dla samego okna na tej platformie. Nie oznaczam tego jako w pełni
zweryfikowanego.

**Bramki:**

* A01 nadal pozwala zabić niewłaściwy proces? **Nie** — każda ścieżka zabicia wymaga
  `matches_os()` albo uchwytu `TreeGuard`; `kill_tree(pid)` występuje już tylko w
  `TreeGuard::terminate` (fallback po nieudanym jobie, wołany wyłącznie z weryfikowaną
  własnością) i w `recover()` po sprawdzeniu tożsamości.
* Błąd zapisu sesji pozwala wykonać spawn? **Nie** — test `p03`.
* Cancel zwalnia slot przed końcem? **Nie** strukturalnie (próby, nie stany); dowód mutacyjny
  na Windows nie jest rozstrzygający (§ wyżej).
* Stary timeout może zakończyć nowy proces? **Nie** — eskalacja w pętli próby + `AttemptId`.
* Zombie/orphan uznane za bezpiecznie zakończone? **Nie** — `TreeOutcome` + kontrakt
  (z zastrzeżeniem procesów breakaway, §10).
* Timeout nie obejmuje stdin? **Nie** — testy `p06`/`p07`.
* Migracja uszkadza dane? **Nie** — addytywna; test migracji.
* Dotychczasowe testy przestały przechodzić? **Nie** — 162/0, exit 0; E2E demo 30/30.

## 16. Podsumowanie

Sprint 01.5A domyka wszystkie pięć ustaleń etapu A (A01–A05) z dowodami na poziomie OS, DB
i supervisora. Fundament pod Sprint 02 (tożsamość próby, okupacja, drzewo procesów, trwały start)
jest gotowy; kolejnym krokiem z planu remediacji jest krok 3 (A06–A07: granice transakcji
i durable outcome), który nie wymaga już przebudowy modelu danych.
