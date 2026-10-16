# FORGE — Independent Sprint 01 Audit

**Data:** 2026-10-16, strefa Europe/Warsaw  
**Repozytorium:** E:\forge  
**Rewizja:** 3f58eecc65a4e1f76e7693732ab842eb6aea5f47  
**Tryb:** audit-only; bez zmian kodu produkcyjnego, zależności, kontraktów, commitów i pushy  
**Decyzja:** **NO-GO dla uruchomienia autonomicznego orchestratora Sprintu 02 na obecnym fundamencie.**

## Executive Summary

Podział na sześć crates, czysty scheduler, oddzielny worker SQLite i rozdzielenie daemona od TUI są dobrym punktem wyjścia. Nie ma potrzeby przepisywania całego projektu. Kontrolowane wykonanie 10 oraz 20 zadań działa, a zwykłe worktrees nie zmieniają plików canonical checkout. To jednak nie wystarcza do zatwierdzenia niezawodności.

Potwierdziłem błędy w granicach transakcji i własności zasobów: plan może zostać zapamiętany bez zadań, zadanie może otrzymać COMPLETED bez artefaktów albo pozostać RUNNING bez supervisora, proces może wystartować bez zapisanej sesji, anulowanie przed faktycznym zatrzymaniem zwalnia limit wykonania, a wygaśnięcie lease może dopuścić konfliktujące zadanie mimo nadal działającego właściciela. Retry rodzica nie odblokowuje dziecka. Recovery ufa samemu PID i może zakończyć niepowiązany proces.

Dwa wskazane problemy TUI zostały potwierdzone przez rzeczywiste funkcje produkcyjne na izolowanej kopii źródeł: pobrany log nie zasila OUTPUT, a zamknięcie transportu bez oczekującego requestu nie zmienia App.connected i nie uruchamia automatycznego reconnect. Dodatkowo fragmentacja poprawnego requestu podczas notyfikacji powoduje utratę części ramki i zamknięcie połączenia.

Wyniki wymaganych kontroli:

| Kontrola | Wynik |
| --- | --- |
| cargo fmt --all -- --check | PASS |
| cargo clippy --workspace --all-targets -- -D warnings | PASS |
| cargo test --workspace | **145 PASS, 0 FAIL po uruchomieniu z działającym tasklist** |
| cargo build --release | PASS |
| scripts/e2e-demo.ps1 na nowym, izolowanym WorkRoot | **30/30 PASS** |
| Własne diagnostyki | Potwierdzone usterki mimo powyższych PASS |

Raport zawiera **21 istotnych ustaleń: 1 P0, 12 P1 i 8 P2**. P0 dotyczy niebezpiecznej identyfikacji procesów. P1 dotyczą integralności wykonania, schedulera i protokołu. P2 obejmują m.in. ograniczenia pamięci, diagnostykę, oba błędy TUI oraz niezweryfikowane natywnie ścieżki Unix.

**NO-GO nie oznacza zakazu dalszej pracy projektowej nad Sprintem 02.** Można rozwijać kontrakt planera i pracować na atrapach. Nie należy jeszcze powierzać obecnemu silnikowi autonomicznego replanningu, retry i integracji kodu bez nadzoru.

## Audit Scope

### Zakres i metoda

Przeanalizowano README, ARCHITECTURE, DECISIONS, SPRINT_01_REPORT, PROGRESS, AGENT_ADAPTERS, manifesty workspace, migrację oraz produkcyjne moduły wszystkich sześciu crates. Krytyczne ścieżki schedulera, dispatch, sesji, wyników, recovery, IPC i TUI sprawdzono przez śledzenie ich rzeczywistych konsumentów. Przejrzano istotne asercje istniejących testów i skrypt E2E.

Część bezpieczeństwa miała dodatkowy niezależny przegląd bazowy oraz osobny przegląd architektury/TUI zgodnie z workflow Codex Security. Nie przeprowadzano wieloprzebiegowego skanu Deep ani audytu wszystkich zależności pod kątem CVE. Ocena niniejszego raportu obejmuje przede wszystkim niezawodność i integralność, a nie tylko podatności z zewnętrznym napastnikiem.

Własne eksperymenty wykonywano przeciw release binary albo przeciw rzeczywistym funkcjom na **tymczasowej kopii workspace**. Dodatkowe testy dopisano wyłącznie do tej kopii. Ich PASS oznacza potwierdzenie opisanej obserwacji, często błędnego zachowania; nie jest deklaracją, że produkt spełnia docelowy kontrakt.

### Środowisko

- Windows x86_64.
- rustc 1.99.1 (58cf8f9c3 2026-10-13), stable-x86_64-pc-windows-gnu.
- Git 2.55.0.windows.3.
- Tokio, rusqlite z bundled SQLite, Ratatui 0.29 zgodnie z Cargo.lock.
- Zainstalowany target Rust: wyłącznie x86_64-pc-windows-gnu.
- Konfiguracje, bazy i repozytoria testowe były oddzielone od stanu użytkownika.
- Atrapy: własne krótkotrwałe procesy Python oraz forge __demo-agent; bez płatnych modeli.

Pierwszy cargo test w ograniczonym sandboxie zatrzymał się na platform::tests::current_process_is_alive. Po ponowieniu z działającym systemowym tasklist wszystkie testy przeszły. Nie traktuję pierwszego wyniku jako potwierdzonej regresji aplikacji; ujawnia on jednak zależność process_alive od zewnętrznego narzędzia i jego uprawnień.

Nie odczytywano sekretów dostawców. Nie wykonywano prób obejścia uwierzytelniania ani operacji na procesach innych użytkowników. Próba nieprawidłowego PID w recovery wskazywała wyłącznie własny, specjalnie uruchomiony proces. Nie wymuszano rzeczywistego ponownego przydzielenia PID przez Windows.

### Mapa krytycznej ścieżki

~~~mermaid
flowchart TD
    C[CLI / TUI] --> I[IPC: Hello, request ID, NDJSON]
    I --> E[Pojedynczy właściciel Engine]
    E --> P[Walidacja i zapis planu / DAG]
    E --> S[Pure scheduler]
    S --> L[Stan LEASED i resource leases]
    L --> G[Git worktree i zapis próby]
    G --> R[Sesja i supervisor procesu]
    R --> O[stdout / stderr / outcome]
    O --> A[Artefakty i stan końcowy]
    E --> D[Worker SQLite]
    P --> D
    L --> D
    A --> D
    D --> REC[Recovery po restarcie]
~~~

Najbardziej niebezpieczne granice leżą między kolejnymi trwałymi zapisami a operacjami OS/Git, a nie wewnątrz samej funkcji schedule.

## Findings

Poziomy pewności:

- **CONFIRMED** — potwierdzenie przez kod i/lub kontrolowaną reprodukcję.
- **HIGH CONFIDENCE** — silna podstawa w kodzie, lecz brak pełnej próby danego scenariusza/platformy.
- **HYPOTHESIS** — niewystarczające dowody; takie kwestie oddzielono od ustaleń.

Severity oznacza skutki według kryteriów użytkownika. Potwierdzona wada operacyjna nie jest automatycznie dowodem podatności dostępnej zdalnemu napastnikowi.

### A01 — PID nie jest tożsamością procesu

- **ID:** A01
- **SEVERITY:** P0 — CRITICAL
- **CONFIDENCE:** CONFIRMED dla decyzji o zabiciu niewłaściwego procesu; HIGH CONFIDENCE dla naturalnego scenariusza reuse PID.
- **MODULE:** forge / forge-runtime / forge-store
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:405–418, 2228; crates/forge-runtime/src/platform.rs:152–179, 185–203; crates/forge-runtime/src/supervisor.rs:233–241; migrations/0001_init.sql:86–95.
- **ROOT CAUSE:** Recovery sprawdza jedynie, czy zapisany numer PID aktualnie istnieje. Nie porównuje czasu utworzenia procesu, tożsamości uruchomienia ani własności uchwytu/job. started_at sesji jest czasem zapisu sesji, a nie zweryfikowaną tożsamością procesu OS. Dodatkowo supervisor uruchamia niepowiązaną z dalszym życiem dziecka eskalację zabicia PID po pięciu sekundach.
- **REPRODUCTION:** W prywatnej bazie zapisano starą aktywną sesję wskazującą własny proces kontrolny, który nie był agentem Forge. Po restarcie daemon zakończył ten proces. Nie zabijano losowych procesów i nie wymuszano OS PID reuse.
- **EVIDENCE:** stale_pid_identity: controlled_unrelated_process_killed=true, task=INTERRUPTED; event stwierdził zabicie osieroconego procesu. Zapis sesji zawierał sztucznie stary started_at=1, co nie przeszkodziło recovery.
- **IMPACT:** Po crashu i ponownym użyciu PID możliwe zakończenie cudzej pracy lub procesu z niezapisanymi danymi. To uzasadnia P0 mimo zależności od czasu i przydziału PID.
- **RECOMMENDED FIX:** W czasie działania używać uchwytów procesu i strukturalnie zarządzanej grupy/job; przy restarcie wymagać mocnej weryfikacji właściciela procesu lub bezpiecznie odmówić automatycznego kill. Eskalacja musi być anulowana po zakończeniu konkretnego uruchomienia. Na Windows rozważyć Job Object z KILL_ON_JOB_CLOSE i przypisaniem dziecka przed rozpoczęciem właściwej pracy. Sam Job Object nie zapewnia sandboxa filesystemu.
- **REGRESSION TEST:** Dostawca tożsamości zwraca ten sam PID z innym creation time — liczba wywołań kill ma wynosić zero. Osobno: dziecko kończy się przed grace deadline — nie następuje późniejsze zabicie jego dawnego PID.
- **BLOCKS SPRINT 02:** YES

### A02 — Proces może wystartować bez trwałej sesji i odzyskiwalnej własności

- **ID:** A02
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge daemon / forge-runtime
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:1547–1557, 1618–1636, 1704–1713; crates/forge-runtime/src/supervisor.rs:183–185.
- **ROOT CAUSE:** Błąd open_session jest zastępowany zerem przez unwrap_or(0), po czym następuje spawn. PID jest trwale zapisywany dopiero po asynchronicznej obsłudze Started. Crash pomiędzy spawn a tym zapisem zostawia proces bez użytecznego PID w bazie.
- **REPRODUCTION:** Trigger na prywatnej bazie odrzucał INSERT do agent_sessions. Złożono zwykłe zadanie z kontrolowanym procesem.
- **EVIDENCE:** session_failure: task.state=RUNNING, session_count=0. Test nie symulował rzeczywistego braku miejsca; deterministycznie wymuszał błąd tej operacji zapisu.
- **IMPACT:** Po awarii brak trwałej informacji pozwalającej odzyskać własność procesu. Recovery oznaczy zadanie INTERRUPTED, lecz proces może nadal działać. Retry może nałożyć nową próbę na starą.
- **RECOMMENDED FIX:** Nie rozpoczynać procesu po błędzie zapisu sesji. Wprowadzić trwały identyfikator próby i protokół startu, którego skutki uboczne pozostają pod kontrolą OS także w oknie między spawn a zapisem PID.
- **REGRESSION TEST:** Odrzucony zapis sesji oznacza brak utworzenia procesu. Fault injection na kolejnych granicach startu musi kończyć się stanem z jednoznacznym właścicielem procesu albo potwierdzonym brakiem procesu.
- **BLOCKS SPRINT 02:** YES

### A03 — Wyjście rodzica nie kończy odpowiedzialności za potomków

- **ID:** A03
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-runtime / recovery
- **FILE / LINE:** crates/forge-runtime/src/supervisor.rs:276–295, 318–340; crates/forge-runtime/src/platform.rs:132–179; crates/forge/src/daemon/engine.rs:385–400.
- **ROOT CAUSE:** child.wait kończy wykonanie supervisora bez stwierdzenia, czy drzewo/grupa nadal ma procesy. Normalny exit nie uruchamia sprzątania pozostałych potomków. CREATE_NEW_PROCESS_GROUP nie daje na Windows gwarancji zabicia grupy po śmierci daemona. Recovery zamyka sesje nieaktywnych zadań bez przejęcia potencjalnie żywego drzewa.
- **REPRODUCTION:** Kontrolny agent uruchomił własnego potomka śpiącego maksymalnie 25 s, przekierował jego standardowe strumienie i sam zakończył się kodem 0.
- **EVIDENCE:** tree_parent_exit: task=COMPLETED, descendant_alive=true. Potomek został następnie zakończony przez audyt, wyłącznie na podstawie PID zapisanej przez tę atrapę.
- **IMPACT:** Zadanie zakończone lub anulowane może nadal wykonywać operacje. Crash daemona i późniejszy restart nie gwarantują odzyskania całego drzewa. Samo oczekiwanie na EOF strumieni też nie rozwiązuje problemu.
- **RECOMMENDED FIX:** Własność całego job/grupy, a nie tylko rodzica. Zdefiniować, czy żyjący potomek po wyjściu rodzica oznacza błąd kontraktu, czy powinien być zakończony. Recovery musi uwzględnić aktywne procesy także przy terminalnym stanie zadania.
- **REGRESSION TEST:** Rodzic tworzy potomka z odłączonym stdout/stderr i kończy się. Forge nie może raportować pełnego zamknięcia próby z żywym potomkiem. Osobno crash wyłącznie daemona oraz cancel przed jego restartem.
- **BLOCKS SPRINT 02:** YES

### A04 — Zapis promptu na stdin omija timeout i cancellation

- **ID:** A04
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-runtime
- **FILE / LINE:** crates/forge-runtime/src/supervisor.rs:187–222.
- **ROOT CAUSE:** stdin.write_all jest awaitowane przed uruchomieniem czytników stdout/stderr, zegara deadline i select obsługującego kill. Agent nieczytający stdin może wypełnić pipe i zatrzymać supervisora w tej fazie. Agent piszący dużo stdout przed czytaniem stdin może dodatkowo zakleszczyć obie strony.
- **REPRODUCTION:** stdin_prompt=true, prompt 2 MiB, timeout_secs=1, własny proces nieczytający stdin i samokończący się po 15 s. Po dwóch sekundach wykonano cancel.
- **EVIDENCE:** stdin_timeout_cancel: zadanie CANCELLED, sesja nadal RUNNING. Kolejna próba potwierdziła przez OS, że stary proces nadal żył.
- **IMPACT:** Deklarowany twardy timeout nie obejmuje całego życia procesu; cancel może zmienić tylko widoczny stan. To może prowadzić do zajęcia slotów, utraty kontroli i nakładania prób.
- **RECOMMENDED FIX:** Uruchomić czytniki natychmiast po spawn; zapis stdin prowadzić równolegle pod tym samym terminem i sygnałem anulowania. Wszystkie zadania pomocnicze muszą zostać zakończone/joinowane.
- **REGRESSION TEST:** Prompt większy niż pojemność pipe i proces nieczytający stdin. Timeout oraz cancel muszą zakończyć własne drzewo procesu w przewidzianym budżecie. Osobno proces najpierw zalewający stdout, potem czytający stdin.
- **BLOCKS SPRINT 02:** YES

### A05 — Cancel zwalnia limity przed potwierdzeniem zakończenia procesu

- **ID:** A05
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge daemon scheduler integration
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:607–613, 963–980, 1247; crates/forge-core/src/model.rs:85–88.
- **ROOT CAUSE:** Limity globalne i per-agent są liczone ze stanów Task, a cancel od razu przechodzi do CANCELLED. RunHandle i proces mogą nadal istnieć. Stan intencji użytkownika zastępuje tym samym informację o zajętym zasobie wykonawczym.
- **REPRODUCTION:** max_concurrency=1 i slots=1. Pierwszy proces blokował się w scenariuszu A04. Po cancel złożono drugie zadanie.
- **EVIDENCE:** cancel_capacity: task_states=[CANCELLED,RUNNING], dwie sesje RUNNING i actual_alive_count=2 przy limicie 1.
- **IMPACT:** Przekroczenie limitów zasobów oraz nakładanie pracy. Na Unix podobne okno występuje również podczas zwykłego oczekiwania na graceful termination.
- **RECOMMENDED FIX:** Osobny stan CancelRequested/Cancelling lub niezależna ewidencja okupacji slotu przez próbę. Zwolnienie slotu dopiero po potwierdzeniu wyjścia i sprzątnięciu drzewa.
- **REGRESSION TEST:** Cancel procesu opóźniającego wyjście nie dopuszcza drugiego wykonania ponad limit. Sprawdzić zarówno limit globalny, jak i slots agenta oraz brak wycieku slotu po błędzie.
- **BLOCKS SPRINT 02:** YES

### A06 — Idempotency key może trwale zapamiętać plan bez zadań

- **ID:** A06
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge / forge-store
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:800–858; crates/forge-store/src/repo.rs:178–199, 318–381.
- **ROOT CAUSE:** create_plan utrwala request_id oddzielnie od transakcji create_plan_tasks. Replay uznaje istnienie rekordu plans za dowód pełnego przyjęcia żądania.
- **REPRODUCTION:** Trigger odrzucał INSERT zadań w prywatnej bazie. Po pierwszym błędzie usunięto trigger i ponowiono to samo żądanie.
- **EVIDENCE:** Pierwsza odpowiedź: internal/sqlite error. Replay: plan_accepted, created=false, tasks=[]. Baza: plans=1, tasks=0.
- **IMPACT:** Utrata zamierzonego wykonania; zwykłe bezpieczne ponowienie nie naprawia sytuacji. Ten sam stan może powstać po crashu między dwoma zapisami.
- **RECOMMENDED FIX:** Jedna transakcja obejmująca key, plan, zadania i edges albo jawny durable stan przyjęcia z odzyskiwalną materializacją. Replay musi sprawdzać integralność i zgodność treści.
- **REGRESSION TEST:** Fault injection przed i po każdym zapisie przyjęcia planu. Replay ma zwrócić kompletny poprzedni wynik albo dokończyć materializację, nigdy zaakceptowany pusty plan.
- **BLOCKS SPRINT 02:** YES

### A07 — Outcome nie jest atomowym kontraktem zakończenia

- **ID:** A07
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge daemon / forge-store
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:1765–1795, 1808–1831, 1869–1890, 1964–1982, 2067–2087.
- **ROOT CAUSE:** Run jest usuwany z ewidencji przed potwierdzeniem trwałego wyniku. Błędy odczytu Git i zapisu artefaktów są ignorowane; artefakty, sesja, metadane końca, stan i leases mają osobne granice zapisu. Nie ma mechanizmu ponowienia/reconciliation dla zakończonego procesu z nieutrwalonym wynikiem.
- **REPRODUCTION:** (1) Trigger odrzucał INSERT task_artifacts. (2) Osobny trigger odrzucał przejście do FAILED po kontrolowanym exit 2.
- **EVIDENCE:** (1) task=COMPLETED, artifact_count=0. (2) task=RUNNING, daemon.running_tasks=0, DB=(RUNNING, exit_code=2). Stan drugi pozostaje bez supervisora; tick iteruje po runs i go nie naprawia.
- **IMPACT:** Fałszywe COMPLETED lub trwały RUNNING bez procesu; zależne zadania mogą ruszyć na podstawie niekompletnego wyniku. Deklaracja dokumentacji „exit 0 AND artifacts recorded” nie jest egzekwowana.
- **RECOMMENDED FIX:** Trwały outcome identyfikowany attempt_id; atomowa finalizacja metadanych/sesji/artefaktów/stanu i zwolnienia leases. Oddzielić wynik procesu od wyniku weryfikacji kontraktu. Po błędzie zapisu zachować stan wymagający rekoncyliacji i ponowienia.
- **REGRESSION TEST:** Odrzucić kolejno zapis artefaktu, close_session, mark_task_finished i zmianę stanu. Nie wolno otrzymać COMPLETED bez wymaganych danych ani pozostawić RUNNING bez właściciela wyniku. Restart musi deterministycznie dokończyć finalizację.
- **BLOCKS SPRINT 02:** YES

### A08 — Udany retry rodzica nie odblokowuje dziecka

- **ID:** A08
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-core scheduler / forge daemon
- **FILE / LINE:** crates/forge-core/src/scheduler.rs:70–95, 110–119; crates/forge/src/daemon/engine.rs:997–1061; crates/forge-core/src/state.rs:41.
- **ROOT CAUSE:** Dependency gating obsługuje CREATED i READY, ale pomija BLOCKED. Legalne przejście BLOCKED→READY istnieje, lecz scheduler go nie emituje, a retry odmawia operacji dla BLOCKED.
- **REPRODUCTION:** Rodzic fail_once zakończył pierwszą próbę błędem. Dziecko z deps=[parent] przeszło do BLOCKED. Retry rodzica zakończył się sukcesem.
- **EVIDENCE:** retry_dependency: [FAILED,BLOCKED] → [COMPLETED,BLOCKED]; attempts rodzica=1. Kolejne ticki nie zmieniły stanu dziecka.
- **IMPACT:** Trwale zgubione zadania zależne po zwykłej, przewidzianej operacji retry. Orchestrator nie może polegać na postępie DAG.
- **RECOMMENDED FIX:** Ponowna ewaluacja BLOCKED wraz z rozróżnieniem powodów blokady; przejście do READY dopiero po spełnieniu wszystkich zależności.
- **REGRESSION TEST:** Failed parent → blocked child → successful retry → child executes once. Osobno kilka zależności, cancellation rodzica i zależność INTERRUPTED.
- **BLOCKS SPRINT 02:** YES

### A09 — Claims nie rozpoznają tożsamości ścieżek Windows

- **ID:** A09
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED dla równoważnych zapisów; HIGH CONFIDENCE dla nieprzetestowanych junction/symlink.
- **MODULE:** forge-core
- **FILE / LINE:** crates/forge-core/src/model.rs:714–753; crates/forge-core/src/scheduler.rs:152–159.
- **ROOT CAUSE:** Claims są porównywane jako tekst. Normalizacja nie ujednolica separatorów, wielkości liter, "." ani "..". Pusty claim katalogu po normalizacji nie obejmuje korzenia projektu.
- **REPRODUCTION:** Wywołano rzeczywiste claims_conflict dla równoważnych nazw Windows i alternatywnej pisowni tej samej ścieżki.
- **EVIDENCE:** file:src/lib.rs kontra file:SRC/lib.rs, file:src\lib.rs i file:./src/lib.rs → false. dir: kontra file:src/lib.rs → false.
- **IMPACT:** Konfliktujące zadania mogą być dopuszczone równocześnie. To naruszenie kontraktu schedulera, nawet jeśli obecne osobne worktrees ograniczają bezpośredni konflikt plików.
- **RECOMMENDED FIX:** Jawna tożsamość zasobu: project ID + bezpieczna ścieżka względna + zadeklarowana semantyka case/separatorów. Rozwiązać "..", odrzucać wyjście poza root, zdefiniować zachowanie dla nieistniejących plików, symlinków i reparse points. Resource opaque powinien mieć odrębnie zdefiniowany namespace.
- **REGRESSION TEST:** Tabela Windows/Unix dla separatorów, case, ".", "..", korzenia, granic prefiksu, symlink/junction oraz różnych projektów. Nie wystarczy test wyłącznie identycznych stringów.
- **BLOCKS SPRINT 02:** YES

### A10 — Częściowo pobrane claims zostają po błędzie kolejnego claimu

- **ID:** A10
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge daemon / forge-store
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:1354–1383; crates/forge-store/src/repo.rs:949–969.
- **ROOT CAUSE:** Claims są pobierane kolejno, poza wspólną transakcją. Przy niepowodzeniu następuje tylko powrót LEASED→READY. Poprzednie claims pozostają w bazie i w pamięci.
- **REPRODUCTION:** Zadanie A wymagało dwóch claims; trigger odrzucał drugi. Zadanie B wymagało pierwszego claimu. Żaden proces nie wystartował.
- **EVIDENCE:** partial_claims: states=[READY,READY], leases=[resource:first→task 1], sessions=0. Kolejne próby A odnawiały pierwszy claim. Dopiero anulowanie A i wygaśnięcie lease pozwoliły B postąpić.
- **IMPACT:** Utrata postępu i blokowanie innych zadań przez zadanie, które w ogóle nie działa. Błąd może utrzymywać się przez dowolną liczbę ticków.
- **RECOMMENDED FIX:** Pobierać cały zestaw claims oraz odpowiedni stan próby atomowo; przy konflikcie/błędzie rollback całego zestawu. Aktualizować mirror dopiero po commit.
- **REGRESSION TEST:** Błąd n-tego claimu nie pozostawia żadnego nowo pobranego lease; zadanie konkurujące o wcześniejszy claim może się uruchomić.
- **BLOCKS SPRINT 02:** YES

### A11 — Wygaśnięcie lease nie odcina nadal żywego właściciela

- **ID:** A11
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge daemon / forge-store
- **FILE / LINE:** crates/forge/src/daemon/engine.rs:2107–2144; crates/forge-store/src/repo.rs:972–993.
- **ROOT CAUSE:** Błąd heartbeat jest ignorowany. expire_leases usuwa rezerwację, a mirror przestaje reprezentować konflikt, choć odpowiedni proces nadal działa. Brak fencing/stop-before-reassign.
- **REPRODUCTION:** Lease TTL=4 s, dwa zadania z resource:shared, pierwszy proces pracował 7 s. Trigger deterministycznie odrzucał aktualizację heartbeat, pozostawiając inne operacje dostępne.
- **EVIDENCE:** lease_expiry_unfenced: states=[RUNNING,RUNNING], obie sesje RUNNING, jedyny aktualny lease należał już do task 2.
- **IMPACT:** Naruszenie wyłączności zasobu po awarii zapisu/odnowienia. Poprawny UNIQUE(resource) w SQLite nie zabezpiecza wykonania, które nadal trwa.
- **RECOMMENDED FIX:** Przy utracie lease zatrzymać/fence poprzednią próbę przed dopuszczeniem następnej. Dla lokalnego pojedynczego silnika można zachować blokadę na podstawie żywego run aż do potwierdzonego zakończenia; same TTL nie powinny nadawać ponownie prawa żywemu konkurentowi.
- **REGRESSION TEST:** Symulacja utraty odnowienia bez zabijania procesu. Przez cały czas najwyżej jeden żywy właściciel zasobu; zwolnienie dopiero po potwierdzonym odcięciu poprzednika.
- **BLOCKS SPRINT 02:** YES

### A12 — Anulowanie read future gubi fragment poprawnej ramki IPC

- **ID:** A12
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-protocol / forge daemon connections
- **FILE / LINE:** crates/forge-protocol/src/framing.rs:43–52; crates/forge/src/daemon/conn.rs:103–105, 172–181.
- **ROOT CAUSE:** Częściowo odczytana ramka znajduje się w lokalnym Vec wewnątrz read(), a nie w trwałym stanie FrameReader. Wybranie innej gałęzi select porzuca future wraz z odczytanym prefiksem. BufReader zachowuje tylko bajty jeszcze nieskonsumowane.
- **REPRODUCTION:** Test duplex: dostarczenie prefiksu JSON, przerwanie read timeoutem i dosłanie reszty → Invalid. Osobno rzeczywiste uwierzytelnione połączenie do izolowanego daemona: częściowy Ping, zwykła notyfikacja wywołana drugim klientem, następnie reszta Ping.
- **EVIDENCE:** audit_partial_frame_cancel: błąd JSON zaczynającego się od pola id bez utraconego prefiksu. fragmented_authenticated_request: first_frame=notify, connection_closed=true, pong_received=false.
- **IMPACT:** Zwykła segmentacja TCP i ruch notyfikacji mogą losowo zrywać poprawne połączenia. Klient może nie wiedzieć, czy mutacja została przyjęta, co dodatkowo obciąża kontrakt idempotencji.
- **RECOMMENDED FIX:** FrameReader musi posiadać trwały bufor częściowej ramki, zachowywany przy anulowaniu. Alternatywnie dedykowany task odczytu; zachować kontrolę limitu przed alokacją.
- **REGRESSION TEST:** Poprawna ramka dzielona na każdy możliwy fragment; pomiędzy fragmentami wymuszona odpowiedź/notyfikacja. Każdy request odebrany dokładnie raz, bez utraty prefiksu i bez zamknięcia połączenia.
- **BLOCKS SPRINT 02:** YES

### A13 — Git czeka na exit przed opróżnieniem stdout/stderr

- **ID:** A13
- **SEVERITY:** P1 — HIGH
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-git / engine
- **FILE / LINE:** crates/forge-git/src/lib.rs:227–240, 276–308; crates/forge-runtime/src/adapter.rs:193–222; crates/forge/src/daemon/engine.rs:1169–1183, 1402–1422.
- **ROOT CAUSE:** run_git odpyta try_wait aż do wyjścia procesu, a dopiero potem wywołuje wait_with_output. Po wypełnieniu pipe Git nie może zakończyć zapisu, a rodzic nie czyta. Limit diff jest stosowany dopiero po odczytaniu całości. Podobny wzorzec występuje w probe/run_capture.
- **REPRODUCTION:** W nowym repozytorium zmodyfikowano jeden plik do ok. 210 KiB tekstu. Wywołano rzeczywistą GitRepo::diff z max_bytes=200.
- **EVIDENCE:** audit_large_diff_pipe: elapsed=60.0142332 s, result=Err(Timeout(60s)). Nie był to duży projekt ani wielogigabajtowy diff.
- **IMPACT:** Zwykły większy diff staje się timeoutem. Engine awaituje pracę Git, więc w tym czasie kolejkują się operacje sterujące, ticki i heartbeat. spawn_blocking chroni wątki Tokio, lecz nie zachowuje responsywności pojedynczego aktora.
- **RECOMMENDED FIX:** Opróżniać oba strumienie równolegle z oczekiwaniem na proces; narzucić limit zbieranego wyjścia i świadome postępowanie po jego osiągnięciu. Długie Git operations prowadzić jako pracę przygotowawczą zwracającą wynik z attempt/generation ID, bez zatrzymywania pętli engine.
- **REGRESSION TEST:** Diff większy od pojemności pipe, duży stderr i bardzo dużo nazw plików. Powrót w krótkim czasie, poprawna truncation, równoczesne GetState/cancel nadal obsługiwane.
- **BLOCKS SPRINT 02:** YES

### A14 — Limity ramek i linii nie ograniczają pamięci podczas odczytu

- **ID:** A14
- **SEVERITY:** P2 — MEDIUM
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-protocol / forge-runtime
- **FILE / LINE:** crates/forge-protocol/src/framing.rs:43–50; crates/forge-runtime/src/supervisor.rs:358–375.
- **ROOT CAUSE:** read_until zbiera całą linię przed kontrolą długości i truncation. Konwersja UTF-8 i kopie tekstu mogą zwiększyć chwilowe zużycie. Bounded channel nie ogranicza bufora przed kanałem.
- **REPRODUCTION:** Kontrolowana ramka 12 MiB bez LF i osobno pojedyncza linia stdout 100 MiB przy max_line_bytes=4096. Próby miały z góry ograniczoną liczbę bajtów; nie doprowadzano do OOM.
- **EVIDENCE:** IPC working set 14 917 632→28 524 544 B, połączenie nadal czekało na LF. long100: zaobserwowany peak 121 458 688 B, wynikowy log tylko 4402 B. Pomiar okresowy jest dolnym ograniczeniem rzeczywistego chwilowego szczytu.
- **IMPACT:** Jeden proces lub klient może zużyć znacznie więcej pamięci niż deklarowane limity; przy 10–20 procesach efekt sumuje się.
- **RECOMMENDED FIX:** Czytać porcjami i egzekwować granicę przed rozrostem bufora. Dla logów ograniczyć/zrzucać nadmiar linii bez zachowywania jej całej; dla protokołu przerwać odczyt po przekroczeniu limitu.
- **REGRESSION TEST:** Limit+1 bez LF/EOF kończy się kontrolowanym wynikiem. Linie 10/100 MiB nie powodują pamięci proporcjonalnej do pełnej długości; niepoprawny UTF-8 nie wywołuje paniki.
- **BLOCKS SPRINT 02:** YES — przed zwiększeniem autonomicznej skali

### A15 — Backpressure usuwa część raw logów, a liczniki nie obejmują wszystkich strat

- **ID:** A15
- **SEVERITY:** P2 — MEDIUM
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge-runtime / engine diagnostics
- **FILE / LINE:** crates/forge-runtime/src/supervisor.rs:254–263, 303–305, 378–380; crates/forge/src/daemon/engine.rs:1730–1740.
- **ROOT CAUSE:** RawLine może zostać odrzucony przez try_send przed zapisem do LogSink. Z kolei odrzucenie znormalizowanego eventu jest ignorowane i nie zwiększa dropped_lines. Tekst diagnostyki sugeruje, że odrzucone linie są zachowane w pliku, co dla tego pierwszego miejsca jest nieprawdą.
- **REPRODUCTION:** Kontrolowane serie około 10 oraz 100 MiB stdout z prawidłowymi krótkimi liniami i dodatkowym niepoprawnym UTF-8 na stderr.
- **EVIDENCE:** flood10: dropped=8678; flood100: dropped=91618. Daemon pozostał responsywny, ale część danych odpadła jeszcze przed rotowanym logiem. To dodatkowe straty ponad zamierzoną retencję .log/.log.1.
- **IMPACT:** Niepełny materiał do ustalenia auth failure/crash lub przyczyn błędu. Brak pewnej informacji, ile zdarzeń nie dotarło do engine.
- **RECOMMENDED FIX:** Rozdzielić trwały/capped strumień logu od stratnej transmisji live albo jawnie zdefiniować log jako stratny. Oddzielne liczniki raw_drop, event_drop, truncation i rotation; re-sync dla klientów.
- **REGRESSION TEST:** Wysłać ponumerowane linie, policzyć wszystkie etapy i zweryfikować tożsamość bilansu. Osobno nasycić kanał normalized events i sprawdzić raport strat.
- **BLOCKS SPRINT 02:** NO — potrzebne w trakcie Sprintu 02

### A16 — Pobrane logi nie zasilają panelu OUTPUT

- **ID:** A16
- **SEVERITY:** P2 — MEDIUM
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge TUI
- **FILE / LINE:** crates/forge/src/tui/mod.rs:260–272; crates/forge/src/tui/app.rs:99–100, 131–132, 218–226; crates/forge/src/tui/ui.rs:323–336; crates/forge/src/tui/render_tests.rs:178–184.
- **ROOT CAUSE:** Okresowy GetTaskLog dla overview tworzy AppMsg::Logs. Handler aktualizuje log_task/log_text dla ekranu LOGS. Renderer OUTPUT czyta inne pola — output_task/output_text — których ten przepływ nigdy nie aktualizuje. Obecny test renderowania ręcznie wpisuje te pola, omijając błędny przepływ.
- **REPRODUCTION:** Na izolowanej kopii wywołano produkcyjne App::apply_msg(AppMsg::Logs) z task=42 i rozpoznawalnym tekstem.
- **EVIDENCE:** audit_output_ingestion: log_task=Some(42), log_text zawiera pobrany tekst, output_task=None, output_text="". Wyszukiwanie potwierdziło brak produkcyjnych przypisań do cache OUTPUT poza inicjalizacją.
- **IMPACT:** Główne okno sugeruje oczekiwanie na wyjście mimo poprawnie działającego agenta i dostępnych logów. Osobny ekran LOGS może działać, dlatego test samego endpointu/log file nie wykrywa usterki.
- **RECOMMENDED FIX:** Jawnie rozróżnić komunikat overview output od pełnego log view albo utrzymywać jedno spójne źródło cache z identyfikatorem zadania. Odmienne limity 60 i 400 linii nie powinny przypadkowo nadpisywać sobie wyników.
- **REGRESSION TEST:** Odtworzyć całą ścieżkę odpowiedź GetTaskLog→mapowanie AppMsg→apply_msg→render overview. W buforze TestBackend musi wystąpić pobrana linia dla wybranego zadania. Po zmianie wyboru opóźniona odpowiedź poprzedniego zadania nie może nadpisać OUTPUT nowego.
- **BLOCKS SPRINT 02:** NO — naprawić przed użyciem TUI jako głównego monitora

### A17 — TUI może na stałe zachować connected po śmierci transportu

- **ID:** A17
- **SEVERITY:** P2 — MEDIUM
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge TUI / forge-protocol client
- **FILE / LINE:** crates/forge-protocol/src/client.rs:81–103; crates/forge/src/tui/mod.rs:88–93, 132–157, 243–294; crates/forge/src/tui/app.rs:234–237.
- **ROOT CAUSE:** Reader klienta ustawia closed i usuwa pending requests. Bridge czeka natomiast na zamknięcie broadcast notifications, którego sender jest nadal utrzymywany przez Client i Arc posiadany przez sam bridge. Bez pending requestu nie powstaje Disconnected. Tick w gałęzi app.connected=true widzi current()=None i nie przechodzi do reconnect.
- **REPRODUCTION:** W teście na kopii źródeł własny serwer protokołu zakończył połączenie po prawidłowym Hello, przy braku pending requestów. Użyto rzeczywistych Client, spawn_bridge i ClientHandle.
- **EVIDENCE:** audit_idle_disconnect: transport_closed=true, App.connected=true, disconnect_message=NONE, current_client=NONE. Timeout oczekiwania bridge był kontrolowany. Odrębna analiza gałęzi tick potwierdza brak próby reconnect w tym stanie.
- **IMPACT:** Użytkownik widzi nieaktualne zadania jako połączone; automatyczny powrót po restarcie daemona może w ogóle nie nastąpić. Awaria zależy od czasu — pending request przy rozłączeniu uruchamia sprawną ścieżkę błędu i może maskować problem.
- **RECOMMENDED FIX:** Jawny sygnał lifecycle transportu niezależny od strumienia notyfikacji; event loop powinien reagować na closed client także przy app.connected=true. Każde połączenie powinno mieć generation/instance ID, aby stary błąd nie czyścił nowego klienta.
- **REGRESSION TEST:** Prawidłowe połączenie i snapshot, brak pending call, odłączenie serwera między tickami: banner musi przejść w disconnected, reconnect do nowej instancji ma nastąpić automatycznie, a snapshot zastąpić stary. Powtórzyć z pending call, restartem i opóźnionym błędem starej generacji.
- **BLOCKS SPRINT 02:** NO — naprawić przed użyciem TUI jako głównego monitora

### A18 — Brak deadline Hello pozwala wyczerpać limit połączeń

- **ID:** A18
- **SEVERITY:** P2 — MEDIUM
- **CONFIDENCE:** CONFIRMED
- **MODULE:** forge daemon IPC
- **FILE / LINE:** crates/forge/src/daemon/mod.rs:92, 123–135; crates/forge/src/daemon/conn.rs:38.
- **ROOT CAUSE:** Permit jest zajmowany przed Hello, a pierwszy odczyt nie ma terminu. Istnienie limitu liczby połączeń nie zapewnia postępu, jeśli wszystkie miejsca mogą być zajęte bez końca.
- **REPRODUCTION:** W izolowanej konfiguracji ustawiono connection_limit=8 i otwarto osiem własnych bezczynnych socketów. Nie wykonywano operacji wymagających tokenu przez te połączenia.
- **EVIDENCE:** ipc_idle_slots: legitimate_blocked=true. Po zamknięciu socketów normalne połączenie i Ping znów działały.
- **IMPACT:** Niedostępne nowe sesje CLI/TUI, w tym cancel/status/shutdown. Ograniczenie jest lokalne; domyślny listener nie jest publiczną usługą internetową.
- **RECOMMENDED FIX:** Deadline handshake, osobny budżet oczekujących sesji i wyraźne zachowanie po jego przekroczeniu. Ustalić także politykę idle/write timeout dla powolnych klientów.
- **REGRESSION TEST:** Zajęcie całego limitu własnymi idle sockets nie może blokować poprawnego klienta po upływie deadline.
- **BLOCKS SPRINT 02:** NO — naprawić przed szerszym użyciem lokalnego IPC

### A19 — Ścieżka Unix ma brakujący import wymaganej metody

- **ID:** A19
- **SEVERITY:** P2 — MEDIUM
- **CONFIDENCE:** HIGH CONFIDENCE; natywna kompilacja NOT RUN.
- **MODULE:** forge-protocol transport
- **FILE / LINE:** crates/forge-protocol/src/transport.rs:11–15, 114–136, szczególnie 121.
- **ROOT CAUSE:** FileType::is_socket pochodzi z unixowego FileTypeExt; trait nie jest importowany. Lokalny import PermissionsExt dotyczy innej metody.
- **REPRODUCTION:** Inspekcja pełnego pliku i wyszukiwanie importu w workspace. Nie instalowano nowego toolchainu ani nie deklarowano powodzenia kompilacji Unix.
- **EVIDENCE:** Wywołanie meta.file_type().is_socket() przy braku std::os::unix::fs::FileTypeExt w zakresie. Windows pomija ten kod przez cfg.
- **IMPACT:** Oczekiwany błąd kompilacji dla Linux/macOS. Deklaracja „Unix zaimplementowany, tylko nieuruchomiony” jest zbyt mocna bez CI.
- **RECOMMENDED FIX:** Uzupełnić import w odpowiednim cfg i uruchomić natywne testy transportu, procesów oraz praw plików na obu platformach.
- **REGRESSION TEST:** cargo check/test na ubuntu-latest i macos-latest; sprawdzić UDS startup, stale endpoint i shutdown.
- **BLOCKS SPRINT 02:** NO dla zamkniętego Windows-only development; YES dla deklaracji wsparcia Unix

### A20 — Windows batch nie zachowuje deklarowanej granicy argumentów

- **ID:** A20
- **SEVERITY:** P2 — MEDIUM dla potwierdzonej niespójności transportu argumentów; potencjalny skutek bezpieczeństwa zależy od konfiguracji i wejścia.
- **CONFIDENCE:** CONFIRMED dla przepływu i istniejącego testu metaznaków; HIGH CONFIDENCE dla nieprzetestowanych skutków.
- **MODULE:** forge-runtime adapters / platform
- **FILE / LINE:** crates/forge-runtime/src/platform.rs:42–62, 110–121; crates/forge-runtime/src/adapter.rs:151–160; crates/forge-runtime/src/generic.rs:68–80; crates/forge-runtime/src/codex.rs:90–95; crates/forge-runtime/src/opencode.rs:87–93; docs/DECISIONS.md:80–82.
- **ROOT CAUSE:** Nierozpoznany .cmd/.bat jest uruchamiany przez cmd.exe. Faktyczne adaptery kierują prompt na stdin tylko przy jawnym stdin_prompt=true. Helper place_prompt, który opisuje silniejsze zachowanie, nie ma konsumentów. Deklarowanej walidacji dynamicznych argv w supervisorze nie ma.
- **REPRODUCTION:** Inspekcja rzeczywistych prepare→supervise→maybe_wrap_script. Istniejący test cmd_metacharacters_do_not_survive_batch_arguments przeszedł i potwierdza zniekształcanie danych. Nie uruchamiano dodatkowej komendy przez spreparowany plan.
- **EVIDENCE:** Domyślne stdin_prompt=false; prompt i wartości szablonów trafiają do args, a następnie do interpretera. Rozpoznanie istniejącego docelowego .exe omija tę ścieżkę i jest ważnym kontrdowodem.
- **IMPACT:** Uszkodzenie promptu/argumentów albo dodatkowa interpretacja przez powłokę w konfiguracji batch. Nie stwierdzam wykorzystania przeciw rzeczywistemu agentowi ani uniwersalnej podatności wszystkich adapterów.
- **RECOMMENDED FIX:** Preferować rzeczywisty executable. Dla wspieranych batch jawnie wymusić bezpieczny transport danych, sprawdzać także dynamiczne title/worktree/extra_args i odrzucać konfiguracje bez gwarancji dosłowności.
- **REGRESSION TEST:** Przygotowanie i uruchomienie nieszkodliwego batch z zestawem znaków specjalnych ma zachować dane bez interpretacji albo kontrolowanie odmówić wykonania. Testować rzeczywistych konsumentów, nie sam nieużywany helper.
- **BLOCKS SPRINT 02:** NO przy egzekwowanym native-executable-only; YES przy dopuszczeniu dowolnych batch

### A21 — Scratch doctor nie ma wyłącznej, losowej własności

- **ID:** A21
- **SEVERITY:** P2 — MEDIUM, warunkowe dla współdzielonego katalogu tymczasowego
- **CONFIDENCE:** HIGH CONFIDENCE; natywny scenariusz Unix NOT RUN.
- **MODULE:** forge doctor
- **FILE / LINE:** crates/forge/src/doctor.rs:564–577, 615.
- **ROOT CAUSE:** Nazwa scratch opiera się wyłącznie na PID. create_dir_all akceptuje istniejący katalog, a zapis README.md podąża za istniejącym linkiem. Brak potwierdzonej własności przed pracą i remove_dir_all.
- **REPRODUCTION:** Analiza statyczna. Nie przygotowywano współdzielonego temp, linków do cudzych plików ani demonstracji nadpisania.
- **EVIDENCE:** std::env::temp_dir().join(format!("forge-doctor-{}", process::id())), create_dir_all, następnie std::fs::write(repo.join("README.md"), "probe\n"). Windowsowy prywatny TEMP zawęża praktyczny scenariusz.
- **IMPACT:** Na systemie ze współdzielonym temp możliwe użycie przygotowanego wcześniej obiektu i nadpisanie pliku, do którego caller ma prawo zapisu. Nie twierdzę, że remove_dir_all rekurencyjnie podąża za symlinkiem katalogu.
- **RECOMMENDED FIX:** Losowy prywatny katalog tworzony atomowo i wyłącznie dla tej operacji, ze strażnikiem własności/cleanup. Brak akceptacji zastanego drzewa jako własnego.
- **REGRESSION TEST:** Bezpieczny test z linkiem wyłącznie do kontrolnego pliku we własnym temp; doctor nie może go zmienić i musi użyć nowego prywatnego katalogu.
- **BLOCKS SPRINT 02:** NO dla obecnego Windows-only testowania; potrzebne przed wspieraniem Unix

## Test Evidence

### Wymagane bramki

| Nazwa | Uruchomienie | Wynik | Uwagi |
| --- | --- | --- | --- |
| Format | cargo fmt --all -- --check | PASS, exit 0 | Oryginalny workspace |
| Clippy | cargo clippy --workspace --all-targets -- -D warnings | PASS, exit 0 | Oryginalny workspace |
| Workspace tests, pierwsza próba | cargo test --workspace w sandboxie | Ograniczenie środowiska | current_process_is_alive nie widział procesu przez tasklist; nie kończono na tym audytu |
| Workspace tests, powtórzenie | cargo test --workspace z działającymi narzędziami OS | PASS: 145/145 | 30 forge + 6 daemon + 38 core + 7 git + 18 protocol + 31 runtime + 15 store |
| Release | cargo build --release | PASS, exit 0 | Release binary użyty w eksperymentach |
| Istniejące E2E | powershell -NoProfile -ExecutionPolicy Bypass -File scripts/e2e-demo.ps1 -ForgeBin target/release/forge.exe -WorkRoot nowy_prywatny_temp | PASS: 30/30 | Root przed wykonaniem zweryfikowany jako nowa ścieżka we własnym temp |

### Niezależne eksperymenty

Polecenia wykonywały skrypty audytowe w prywatnym temp, nie pliki produkcyjne. Poniższe identyfikatory jednoznacznie wiążą obserwacje z Findings. Błędy SQLite wymuszano triggerami w osobnych bazach; to nie jest dowód wykonania fizycznego disk-full lub power-loss.

| Nazwa / mechanizm | Sposób | Wynik | Dowód / ograniczenie |
| --- | --- | --- | --- |
| stress10 | 10 zadań, jedna atrapa, 10 slots, limit 10 | PASS | Peak RUNNING=10; wszystkie COMPLETED; 8,47 s; canonical clean |
| stress20, pierwsza próba | 20 zadań, limit klienta 4 s | Timeout obserwatora | Przygotowanie całej partii zatrzymywało odpowiedź; nie uznano za dowód utraty zadań |
| stress20_retry_longer_observer_budget | Nowa baza/repo, 20 slots, dłuższy timeout obserwacji | PASS funkcjonalny | Peak RUNNING=20, wszystkie COMPLETED, 14,62 s; max latency GetState=4,28 s |
| same_claim | 3 zadania z identycznym resource:db, limit 3 | PASS | Max overlap cykli zadania=1 |
| retry_dependency | Fail-once parent, child, retry | FAIL kontraktu | Parent COMPLETED, child nadal BLOCKED — A08 |
| parallel_duplicate | 4 równoległe zwykłe żądania z tym samym request_id | PASS w normalnym przypadku | 1 plan, 1 task; jedna odpowiedź created=true |
| plan_atomicity | Odrzucony INSERT tasks, potem replay | FAIL | 1 plan, 0 tasks, replay zaakceptowany — A06 |
| artifact_atomicity | Odrzucony INSERT artifacts | FAIL | COMPLETED i 0 artefaktów — A07 |
| outcome_failure | Odrzucona zmiana stanu na FAILED po exit 2 | FAIL | RUNNING, exit_code=2, live supervisors=0 — A07 |
| session_failure | Odrzucony INSERT session | FAIL | RUNNING, session_count=0 — A02 |
| partial_claims | Błąd drugiego claimu | FAIL | READY/READY, claim pierwszego nadal task 1, bez sesji — A10 |
| lease_expiry_unfenced | Odrzucone heartbeat, TTL 4 s, żywy agent | FAIL | Dwa RUNNING z tym samym zasobem — A11 |
| stdin_timeout_cancel | Prompt 2 MiB, nieczytający proces, timeout 1 s | FAIL | Cancel zmienił stan, proces/sesja trwały — A04 |
| cancel_capacity | Cancel powyższego + nowe zadanie, limit 1 | FAIL | Dwa żywe własne procesy — A05 |
| stale_pid_identity | Własny proces kontrolny jako stary PID sesji | FAIL | Recovery go zakończył — A01; brak rzeczywistego OS reuse |
| tree_parent_exit | Własny potomek z odłączonymi strumieniami | FAIL | COMPLETED mimo żywego potomka — A03 |
| ipc_oversize_no_lf | Maks. 12 MiB bez LF | FAIL ograniczenia odczytu | Wzrost working set, oczekiwanie na LF — A14 |
| ipc_idle_slots | 8 własnych idle sockets przy limicie 8 | FAIL dostępności | Poprawny klient odrzucony; powrót po zwolnieniu — A18 |
| ipc_reconnect / malformed | Nowy klient po odłączeniu, błędny JSON/typ ramki | PASS | Daemon nadal odpowiadał Pong; to nie jest test reconnect TUI |
| flood10 | Krótkie linie stdout, ok. 10 MiB | PASS żywotności, FAIL kompletności | Peak ~15,85 MiB; latency ≤0,219 s; dropped=8678 |
| flood100 | Krótkie linie stdout, ok. 100 MiB | PASS żywotności, FAIL kompletności | Peak ~16,72 MiB; latency ≤0,219 s; dropped=91618 |
| long100 | Pojedyncza linia 100 MiB | FAIL ograniczenia pamięci | Peak ≥115,83 MiB; truncation dopiero po odczycie — A14 |
| Invalid UTF-8 | Kontrolne bajty stderr w flood | PASS braku paniki | Konwersja lossy; nie dowodzi pełnej kompletności logów |
| retry_base | Zmiana canonical HEAD między próbami | Zmiana bazy potwierdzona | Retry bierze nowy HEAD; poprzedni katalog r0 i README zachowane |
| case_project_collision | Dwa własne repo, projekty audit i AUDIT | FAIL drugiego wykonania, bez nadpisania | Drugi worktree creation odrzucony przez Git, pierwszy zachowany |
| audit_output_ingestion | Produkcyjne App::apply_msg na kopii | FAIL funkcji OUTPUT | log_task ustawione, output_task=None — A16 |
| audit_idle_disconnect | Produkcyjny Client + spawn_bridge + ClientHandle z własnym serwerem | FAIL lifecycle TUI | closed=true, connected=true, brak komunikatu — A17 |
| audit_render_dimensions_unicode | 8 szerokości × 7 wysokości × 6 ekranów | PASS: 336 renderów | W tym 0/1/2 i 80×24, Unicode/combining; bez prawdziwych interaktywnych resize |
| audit_partial_frame_cancel | Duplex, przerwanie częściowego read, reszta ramki | FAIL cancellation safety | Invalid JSON po utracie prefiksu — A12 |
| fragmented_authenticated_request | Własny daemon, częściowy Ping przeplatany notyfikacją | FAIL niezawodności | Połączenie zamknięte, brak Pong — A12 |
| audit_large_diff_pipe | Rzeczywisty GitRepo::diff, jeden plik ~210 KiB, limit 200 B | FAIL | Timeout po 60,014 s — A13 |

Pomiary working set są obserwacjami jednego środowiska, nie benchmarkiem wydajności. Testy 10/20 używały jednego typu adaptera z wieloma slots; nie stanowią dowodu zachowania 20 różnych, kosztownych CLI modeli.

Własne testy Rust uruchomiono jako cargo test --offline --manifest-path tymczasowa_kopia/Cargo.toml --workspace --lib audit_ -- --nocapture. Sześć diagnostyk potwierdziło opisane obserwacje. Ostrzeżenie unused import w dopisanym tymczasowym teście nie dotyczy oryginalnego Clippy.

### Scenariusze ST-01–ST-12

| Scenariusz | Status | Zakres dowodu |
| --- | --- | --- |
| ST-01, 10 zadań | RUN / PASS normalnej ścieżki | 10 równoległych stanów wykonania |
| ST-02, 20 zadań | RUN / PASS z zastrzeżeniem responsywności | 20 zakończonych zadań, 4,28 s blokady odpowiedzi |
| ST-03, ten sam claim | RUN / PASS normalnie, FAIL przy błędzie heartbeat | Zwykła serializacja i A11 |
| ST-04, crash podczas startu | Dokładny moment NOT RUN | Alternatywa: A02, wymuszony błąd sesji; okno spawn→PID potwierdzone kodem |
| ST-05, crash podczas zapisu wyniku | Dokładny moment NOT RUN | Alternatywa: A07, deterministyczne błędy artefaktów i stanu |
| ST-06, proces ignorujący zakończenie | Natywne sygnały Unix NOT RUN | Alternatywa Windows: A04, proces nieczytający stdin, oraz istniejące testy timeout/cancel |
| ST-07, output flooding | RUN | 10/100 MiB, długa linia, invalid UTF-8, brak fizycznego OOM |
| ST-08, cancel i retry | RUN częściowo | Zwykły cancel, ponowienia planu, retry rodzica i po restarcie; brak długiego losowego soak |
| ST-09, disconnect/reconnect TUI | RUN | Błąd idle disconnect potwierdzony; E2E testuje osobne uruchomienia TUI |
| ST-10, nieczyste zamknięcie | RUN | Istniejący test zabija tylko własny daemon; E2E ma słabszą semantykę opisaną niżej |
| ST-11, równoległy ten sam plan | RUN | 4 klientów, jedna materializacja przy braku błędu zapisu |
| ST-12, duże/uszkodzone IPC | RUN | Kontrolowane rozmiary, fragmentacja, poprawny uwierzytelniony Ping; brak obchodzenia auth |

## Concurrency & Recovery Assessment

### Scheduler i maszyna stanów

Pure scheduler jest deterministyczny dla tego samego wejścia. Poprawnie uwzględnia wyższy priorytet, stabilną kolejność, capabilities, preferred agent i liczbę slots w normalnym stanie. W jednym przebiegu ogłasza nowo pobrane claims, więc nie dopuszcza dwóch identycznych claims z tej samej partii. Potwierdzono zwykłą serializację i wykonanie 10/20 zadań.

Słabość leży w zasilaniu schedulera stanem engine: TaskState nie jest wystarczającą ewidencją żywych procesów (A05), leases mogą przestać odpowiadać żywym właścicielom (A11), a BLOCKED nie podlega ponownej ewaluacji (A08). CAS stanu w repo::set_task_state chroni pojedynczy UPDATE, ale nie całą operację „claim→worktree→session→spawn→outcome”.

Legalne przejścia są jawne. Większość unwrap/expect występuje w testach lub dotyczy konstrukcyjnych invariantów map w DAG; nie znalazłem dowodu, że samo ich występowanie uzasadnia ważniejsze ustalenie niż opisane awarie. W engine istotniejszym wzorcem są ignorowane Result i wartości zastępcze. Nie wykazano ogólnej race condition pamięci Rust ani deadlocku na mutexach; wykazano natomiast zakleszczenie pipe Git i logiczne utraty postępu.

Priority scheduling nie ma aging ani deklarowanej gwarancji bounded waiting. Przy ciągłym dopływie zadań o wyższym priorytecie niższe mogą czekać bez końca. Dla skończonego planu nie jest to samo w sobie błąd; przed generowaniem ciągłej pracy przez orchestratora trzeba wybrać i przetestować politykę fairness.

### Odpowiedzi na SC-01–SC-06

| Pytanie | Ocena |
| --- | --- |
| SC-01: double execution | Normalny pojedynczy engine + replay nie stworzył duplikatu w próbie 4 klientów. Brak fencing procesu, utrata sesji, retry po orphan_action=report i niezamknięte stare drzewa nie pozwalają zagwarantować jednego realnego wykonania. |
| SC-02: lost task | **Tak:** BLOCKED po udanym retry oraz RUNNING z exit_code=2 i runs=0 zostały odtworzone. |
| SC-03: zależności po failure/cancel | Zwykłe gating wymaga wszystkich COMPLETED i blokuje Failed/Cancelled. INTERRUPTED czeka na decyzję/retry. Głównym potwierdzonym błędem jest brak odblokowania, a nie start po Failed. Fałszywe COMPLETED z A07 podważa jednak semantyczną wiarygodność warunku. |
| SC-04: cancel races | Jednowątkowy właściciel eliminuje równoczesne mutacje pamięci, lecz cancel nie jest przetwarzany podczas awaitowanej długiej operacji engine. Cancel przed outcome ustawia terminalny stan, choć proces trwa. Jeśli outcome zostanie obsłużony pierwszy, późniejszy cancel terminalnego zadania jest no-op. Brak osobnej tożsamości próby/outcome. |
| SC-05: fairness | Stabilna kolejność przy równym priorytecie; brak aging i gwarancji braku starvation przy ciągłym dopływie wyższego priorytetu. |
| SC-06: resource claims | Zwykły exact claim działa. Aliasy Windows, częściowe pobranie i utrata heartbeat naruszają zamierzone invarianty — A09–A11. |

Dwa cancel dla już terminalnego zadania są no-op na poziomie żądania. Nie oznacza to, że drzewo procesu zostało już zamknięte. Retry dotyczy FAILED/INTERRUPTED i zwiększa attempts w transakcji, lecz bez pełnej izolacji tożsamości próby na wszystkich kanałach.

### SQLite i trwałość

Mocne strony: WAL, foreign_keys=ON, busy_timeout=5 s, transakcyjna migracja z user_version, odmowa nowszego schema, jedna dedykowana nitka writer i kanał 1024 jobs. SQL korzysta z parametrów. Testy pokrywają reopen, zwykłe konkurencyjne wywołania i stan schema.

synchronous=NORMAL chroni spójność SQLite, ale nie stanowi obietnicy utrzymania każdego ostatnio potwierdzonego zapisu przy awarii zasilania. Nie wykonano fizycznego power-loss. Poprawność pliku SQLite nie oznacza spójności między SQLite, Git i procesem OS.

Najważniejsze granice wymagające korekty:

1. Plan i jego materializacja — A06.
2. Trwała sesja przed rozpoczęciem pracy — A02.
3. Pełny zestaw claims i stan przydziału — A10.
4. Wynik, artefakty, sesja i stan końcowy — A07.
5. Utrata prawa do zasobu i zatrzymanie starego właściciela — A11.

BUSY może na pięć sekund wstrzymać oczekiwany przez engine zapis. Kanał worker jest bounded, lecz engine przeważnie wykonuje await przed kolejnym krokiem; sama pojemność kanału nie gwarantuje responsywności. Panic closure zakończy worker, a Store::call potrafi zwrócić WorkerGone; dalsze zachowanie wielu ścieżek nadal polega na ignorowaniu błędów. Dokładny crash DB workera i nasycenie kolejki nie były osobno odtwarzane.

Recovery iteruje po active i REVIEW_PENDING, zamyka sesje, zwalnia claims i zachowuje worktrees. Nie jest atomową rekoncyliacją. Reason dla Interrupted jest ustawiany w pamięci i evencie, ale widoczny kod zmiany stanu nie utrwala go tym samym zapisem w polu failure_reason; nie należy uznawać samego aktualnego snapshotu za dowód kompletnego durable recovery. To należy objąć testem trwałego odczytu po drugim restarcie przy wdrażaniu A07.

### Własność i cancellation safety

Procesy oraz pomocnicze tokio::spawn nie mają jednolitego nadrzędnego lifecycle. RunHandle.join nie jest rutynowo joinowany przy outcome; opóźniona eskalacja i czytniki porzucone przez timeout mogą żyć poza zakresem próby. Błąd A12 pokazuje, że również same future I/O wymagają jawnego kontraktu anulowania.

Engine używa biased select z żądaniami sterującymi przed eventami/outcomes/tick. Przy stałym nacisku kanałów wyższych gałęzi nie ma dowodu bounded latency dla niższych. Nie odtwarzano nieskończonego flood sterowania; raport nie przedstawia tej możliwości jako potwierdzonego permanentnego deadlocku.

## Security Assessment

### Rzeczywiste granice zaufania

Forge jest lokalnym narzędziem jednej instalacji. Token TCP daje dostęp do całego interfejsu daemona: planów, logów, cancel i shutdown. Nie ma niezależnej roli agenta, read-only klienta ani izolacji tenantów. Nie należy przypisywać obecnemu IPC gwarancji, których nie deklaruje.

Windows domyślnie używa loopback z losowym portem i tokenem 32 losowych bajtów. Kontrola tokenu i wersji istnieje. Unixowy socket jest projektowany jako 0600 w prywatnym katalogu. Helper owner-only jest na Windows no-op, więc prywatność niestandardowego data/log/worktree root zależy od rzeczywistych ACL. Nie badano konfiguracji ACL użytkownika i nie stwierdzam uniwersalnego ujawnienia tokenu.

Obejście uwierzytelniania nie było wykonywane ani wykazane. A14 i A18 dotyczą ograniczania zasobów na etapie odczytu połączenia. A12 dotyczy normalnej, prawidłowo uwierzytelnionej komunikacji.

### Agent jest procesem użytkownika, a worktree nie jest sandboxem

Supervisor ustawia cwd i dodatkowe env, ale nie czyści środowiska i nie obniża uprawnień. Agent może, w granicach praw użytkownika, odczytać pliki poza worktree, zmienić canonical checkout, sięgnąć do wspólnego .git, używać sieci i lokalnego IPC. Nie wykonywano takich operacji na danych użytkownika. Ta granica wynika z kodu i jawnej dokumentacji, nie jest ukrytą izolacją zapewnianą przez working directory.

Zmienne środowiskowe mogą też wpływać na Git i agent CLI. Git ustawia trzy zmienne dotyczące promptów/pagera/locks, co nie jest pełnym deterministycznym env ani filtrem sekretów. Nie odczytywano credential stores. Raw logi i stderr tail nie przechodzą przez uniwersalną redakcję; adapterowe redact_secrets jest heurystyką dotyczącą części prezentacji polecenia. Nie należy obiecywać, że output dostawcy nigdy nie zawrze informacji poufnej.

Repozytoryjne artefakty, opisy zadań i wynik modelu należy traktować jako niezaufane dane. Prompt „nie wychodź poza worktree” jest wskazówką, nie mechanizmem kontroli. Model generujący plan musi podlegać deterministycznemu schema/policy validatorowi oraz limitom budżetu, wielkości i uprawnień.

### Shell i terminal

Git oraz natywne executables używają argument arrays. Wyjątek .cmd/.bat opisano w A20; dokumentacja D9 wyprzedza rzeczywistą implementację.

TUI Ratatui nie jest równoważne surowemu drukowaniu ANSI. Lokalny kod Ratatui filtruje znaki kontrolne przy tworzeniu bufora; nie wykazano wykonania OSC przez TUI. Natomiast CLI logs przekazuje raw log na terminal przez println (crates/forge/src/commands.rs:500–515; crates/forge/src/util.rs:61–69), a status/dry-run nie sanitizują wszystkich danych tekstowych. Zalecane są bezpieczny domyślny widok terminalowy i jawny tryb raw. Nie przeprowadzano demonstracji sterowania terminalem ani nie stwierdzam automatycznie wykonania komendy OS przez wyświetlenie tekstu.

### Realistyczne warianty izolacji

Poniższe pozycje są propozycjami, nie gwarancjami obecnego Forge. Dokumentację platform sprawdzono pomocniczo; wnioski dotyczące kodu repozytorium pochodzą z lokalnego przeglądu.

| Platforma | Minimalna kontrola procesu | Dalsza izolacja kodu | Koszt i ograniczenia |
| --- | --- | --- | --- |
| Windows | Job Object obejmujący potomków, KILL_ON_JOB_CLOSE, kontrola handle inheritance i breakaway | Dedykowana tożsamość z ograniczonymi ACL oraz środowiskiem; silniejszy wariant: osobne środowisko VM/WSL/container z kontrolowanymi mountami i credentials | Job rozwiązuje część lifecycle, **nie** ogranicza sam filesystemu/sieci. Zgodność z CLI i uwierzytelnianiem wymaga osobnych testów. [Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects) |
| Linux | Process group i reap; cgroup dla agregacji życia/limitów | Namespaces/mounty oraz Landlock z wykrywaniem ABI, seccomp i ograniczoną siecią zależnie od backendu | Musimy sprawdzić dostępne prawa i ABI; pojedynczy mechanizm nie pokrywa każdej operacji. [Linux Landlock](https://docs.kernel.org/userspace-api/landlock.html) |
| macOS | Grupa procesów i poprawne reaping | Zgodny z podpisywaniem launcher/helper z App Sandbox albo oddzielna VM dla dowolnych CLI | Dziedziczenie sandboxa i entitlements trzeba zaprojektować dla konkretnego sposobu dystrybucji; samo cwd tego nie daje. [Apple App Sandbox](https://developer.apple.com/documentation/security/protecting-user-data-with-app-sandbox) |

Rekomenduję najpierw rozdzielić runtime_state daemona od katalogów dostępnych agentowi, ograniczyć dziedziczone env i zdefiniować backend izolacji z jawnym poziomem gwarancji. Mocniejsza izolacja powinna mieć przewidywalny fallback/odmowę, a nie ciche uruchomienie bez zabezpieczeń.

Możemy wdrażać to etapami: lifecycle i transakcje najpierw, potem jeden w pełni przetestowany backend OS, następnie szersze integracje. Alternatywą na etap eksperymentalny pozostaje zaufany agent pod nadzorem operatora, lecz wówczas musimy jasno utrzymać oznaczenie braku sandboxa i nie traktować claims jako egzekwowanych praw.

## Git Isolation Assessment

### Co rzeczywiście działa

Próby i istniejące E2E potwierdziły osobne worktrees dla zwykłych zadań, brak przypadkowej zmiany plików canonical checkout oraz zachowanie zakończonych worktrees. Retry tworzy nowy numer próby i nie usuwa poprzedniego katalogu. Automatycznego merge, push ani usuwania rezultatów zadań nie ma.

Brak wykazanego shell injection w podstawowej ścieżce Git: argumenty są tablicą, base pochodzi z odczytu HEAD, nazwy zadania są generowane. Bieżący engine serializuje przygotowanie worktrees, a Git ma własne blokady metadanych. To nie jest jednak dowód poprawności dowolnej równoległej integracji wielu daemonów.

### Ograniczenia

- GitRepo::detect używa show-toplevel. Jego komentarz o odmowie linked worktree jest silniejszy niż faktyczna kontrola; nie ma porównania z canonical common Git root.
- ensure_inside istnieje, lecz nie jest używane w produkcyjnym dispatch. Canonicalize po utworzeniu katalogu nie jest sprawdzeniem, że wynik nadal mieści się w zamierzonym root. Junction/symlink scenariusze pozostają bez natywnej walidacji.
- Nazwa projektu jest częścią ścieżki i branch. Na Windows dwa zarejestrowane projekty audit i AUDIT wskazujące różne repo kolidowały w nazwie katalogu. Git odmówił utworzenia drugiego worktree, zachowując pierwszy: zaobserwowano brak postępu, nie utratę danych. Docelowo używać trwałego project_id w katalogach i wykrywać kolizje przy rejestracji.
- Gdy branch już istnieje, blocking closure dopisuje timestamp, ale zwracany payload nie zawiera zmienionego branch; dalszy kod może zapisać pierwotną nazwę. To wymaga poprawnej zwrotnej struktury wyniku i testu collision podczas porządkowania tożsamości prób.
- Retry odczytuje aktualny canonical HEAD, nie poprzedni task.base_commit. W próbie między próbami zmieniono canonical HEAD i base faktycznie się zmienił. To nie jest utrata starego worktree. Jest to niedookreślona semantyka powtórzenia: orchestrator musi wybierać „ta sama baza” lub jawny rebase/replan.
- worktree_remove(force=true) może usuwać dirty work, gdy caller jawnie wybierze force. Nie ma obecnie automatycznego engine cleanup wywołującego tę ścieżkę. Nie wykazano skasowania cudzych zmian.
- Częściowy crash pomiędzy git worktree add a insert_worktree może zostawić rejestrację i katalog bez rekordu bazy. Nie ma kompletnej rekoncyliacji Git registry↔filesystem↔DB.
- agent może użyć Git commit albo edytować wspólną konfigurację mimo instrukcji w prompt. Zwykły diff względem bieżącego HEAD nie jest wystarczającą weryfikacją rezultatu względem niezmiennej bazy próby.

### Claims enforcement i przyszła integracja

Obecnie claims są deklaracjami koordynacyjnymi. Nikt nie porównuje rzeczywistego diff z dozwolonym write scope. Ukończenie procesu nie oznacza zatwierdzenia zmian.

Docelowy, proporcjonalny kontrakt:

~~~mermaid
flowchart LR
    D[Declared write scope + base SHA] --> X[Osobna próba wykonania]
    X --> V[Actual diff vs immutable base]
    V --> C[Kontrola ścieżek, zmian i artefaktów]
    C --> R[Niezależny review]
    R --> Q[Kontrolowana integracja z CAS target SHA]
~~~

Weryfikacja powinna uwzględniać rename/delete, pliki binarne, untracked, staged, zmieniony HEAD, symlink i case Windows. Jeżeli agent może commitować, porównanie musi nadal dotyczyć immutable base. Review i integration nie powinny dziedziczyć uprawnień procesu wykonawczego do sterowania całym daemonem.

## Architecture Assessment

### Granice crates

Kierunek zależności jest acykliczny: forge komponuje pozostałe pięć crates, a store/git/runtime/protocol zależą z workspace jedynie od core. Jest to sensowna granica dla skali 10–20 agentów. Core zawiera także config/path IO, więc określenie całego crate jako pozbawionego I/O należy ograniczyć do modelu schedulera/DAG.

AgentAdapter zapewnia użyteczne prepare/probe/classify hooks. AgentKind i adapter_for są zamkniętym enum/match; nowy typ adaptera wymaga zmiany kodu, podczas gdy generic daje integrację konfiguracyjną. Na obecną skalę nie ma potrzeby budowania dynamicznego plugin ABI.

### Co wymaga zmiany strukturalnej

Pojedynczy właściciel silnika jest zaletą. Nie rekomenduję zastępowania go wspólnym mutable stanem wielu scheduler threads. Wymaga on natomiast rozdzielenia intencji, trwałego stanu próby i rzeczywistych zasobów OS:

- Task/plan opisuje pracę i zależności.
- Attempt posiada niezmienną tożsamość, base SHA, konfigurację wykonania, sesję i proces/job.
- Run occupancy trwa aż do potwierdzonego zamknięcia.
- Outcome i artefakty są utrwalane idempotentnie.
- Recovery rekoncyliuje stan na podstawie tych tożsamości, nie tylko TaskState/PID.

Podczas 20 zadań engine awaitował seryjne przygotowanie worktrees przez 4,28 s widoczne dla GetState. To koszt architektury pętli, nie dowód niedoboru wątków Tokio. Wolne Git/capture powinno wracać przez kontrolowany result channel z generation ID. Poprawki A12/A14 muszą zachować cancellation safety i granice pamięci także na tym kanale.

Nie ma potrzeby optymalizowania SQLite na wielu writerów. Ważniejsze są kompletne transakcje, tryb degraded po awarii storage i odtwarzalna finalizacja. Kanały bounded liczbą elementów nie są budżetem bajtowym, jeżeli pojedynczy element może zawierać duży plan/snapshot/tekst.

Wzrost historii zadań zwiększa koszt pełnych snapshotów i sortowania wszystkich zadań przy tick. Dla bieżących kilkunastu zadań nie jest to priorytet. Dla długotrwałego daemona przyda się ograniczenie aktywnego zbioru, paginacja historii i wersjonowane przyrosty, zanim globalny snapshot przekroczy limit IPC.

### TUI

336 renderów potwierdziło brak paniki w sprawdzonych wymiarach, także 80×24 i skrajnie małych, przy Unicode i combining characters. Nie jest to natywny test wszystkich zdarzeń terminala. Cancel confirmation przechowuje wybrany task ID; request rozwiązuje projekt na jego podstawie, więc zmiana selekcji nie retargetuje bezpośrednio potwierdzenia.

Okno odświeża render po każdym komunikacie i tick. Ratatui wysyła różnice bufora, zatem nie należy utożsamiać tego z wypisywaniem całego terminala za każdym razem. Obecne filtrowanie informacyjnych output events ogranicza napływ do TUI. Nie przeprowadzono benchmarku renderu pod intensywnym strumieniem warning/error.

Snapshoty i odpowiedzi nie mają sprawdzanej wersji/generacji: starszy snapshot może przejściowo zastąpić nowszą notyfikację, a stary błąd połączenia może wyczyścić nowego klienta. Przy korekcie A16/A17 należy objąć tym samym kontraktem target task, connection generation i revision snapshotu.

Przy reconnect ensure jest awaitowane z event loop. Endpoint akceptujący TCP, lecz nieodpowiadający Hello, może opóźnić obsługę klawiatury do request timeout. Potrzebny reconnect worker z terminem i zachowaniem responsywności interfejsu.

## Idempotency and Execution Identity

Obecny content_request_id używa dwóch DefaultHasher, przy czym drugi obejmuje znacznik, długość zawartości i projekt, a nie niezależny hash całej treści. Nie jest to stabilny, jawnie wersjonowany format tożsamości. Nie należy traktować wyniku jako 128-bitowej gwarancji kryptograficznej.

| Pytanie | Odpowiedź oparta na kodzie |
| --- | --- |
| Czy ponowienie tego samego żądania jest bezpieczne? | W zwykłej ścieżce tak; 4 równoległych klientów dało jedno wykonanie. Po częściowym zapisie nie — A06. |
| Czy można świadomie uruchomić identyczny plan drugi raz? | Tak przez nowe jawne request_id w planie/IPC. Bez niego identyczny plik daje replay; CLI nie oddziela naturalnie „nowy run” od „ponów transport”. |
| Czy whitespace zmienia tożsamość? | Tak, hashowane są surowe contents, więc komentarz/newline zmienia key mimo tej samej semantyki TOML. |
| Czy hash obejmuje projekt, config i intencję? | Obejmuje contents i project po ewentualnym CLI override; nie obejmuje skutecznej konfiguracji agentów, repo HEAD, polityki ani intencji nowego run. |
| Czy równoległe żądania tworzą duplikat? | Nie odtworzono tego przy jednym działającym engine. Nie jest to pełny dowód exactly-once skutków procesu i nie obejmuje utraty odpowiedzi po częściowym zapisie. |
| Czy retry zachowuje semantykę? | Zwiększa attempts i zachowuje stare worktree. Baza bierze aktualny HEAD, artefakty i sesje są związane głównie z TaskId, a dziecko pozostaje BLOCKED — wymaga doprecyzowania. |

plans.request_id jest globalnie UNIQUE; replay wyszukuje po samym key bez porównania digestu/specyfikacji ani projektu. Pomyłkowe użycie tego samego jawnego key dla innej treści/projektu zwróci poprzedni plan, zamiast konfliktu. Nie jest to dowód naruszenia uprawnień między tenantami, bo takich tenantów obecnie nie ma; jest to niewystarczający kontrakt execution identity.

Proponowane rozdzielenie, bez wdrażania:

| Tożsamość | Znaczenie |
| --- | --- |
| Specification identity | Digest kanonicznej specyfikacji + schema version; opisuje zamierzony produkt |
| Plan identity | Niezmienna wersja konkretnego DAG/polityki/base references oraz powiązanie ze specyfikacją |
| Run identity | Nowe świadome uruchomienie planu, z projektem, config/policy snapshot i budżetami |
| Idempotency key | Klucz ponowienia pojedynczej operacji transportowej w określonym zakresie; mapuje do run/request digest i odrzuca rozbieżną treść |
| Task attempt identity | Odrębna każda próba; sesje, process identity, worktree, base, leases, outcomes i artefakty wskazują attempt |

Stare próby pozostają audytowalne. Opóźniony wynik próby N nie może zmienić stanu próby N+1. Tego invariant nie zapewnia samo podawanie TaskId na kanałach supervisor events/outcomes.

## Test Quality Audit

Istniejące testy mają realną wartość: uruchamiają SQLite, Git i procesy; sprawdzają CAS transition, migracje, capabilities, zwykłą izolację worktrees i lifecycle daemona. Nie są jedynie atrapą całego systemu. 145 PASS jest poprawnym wynikiem uruchomienia w sprawdzonym środowisku.

Istotne słabości dowodowe:

- Test OUTPUT renderuje ręcznie ustawiony cache, omijając produkcyjne mapowanie Logs — dlatego przechodzi mimo A16.
- Test protocol disconnect ma pending calls. Nie sprawdza zamknięcia strumienia lifecycle przy braku pending requestów — A17.
- high_volume_output_is_handled_without_unbounded_buffering uruchamia własny test binary z --list, czyli bardzo krótki output kilku nazw. Nie wykonuje rzeczywistego flood 10/100 MiB ani linii bez LF.
- Oversized frame test sprawdza odrzucenie gotowych danych. Nie wykrywa wzrostu bufora przed newline ani utraty części ramki podczas select.
- Skrypt E2E nazywa nowo uruchomioną drugą sesję TUI „reconnect”. Nie sprawdza, czy ta sama instancja TUI przeżyje śmierć i restart daemona.
- E2E crash używa taskkill z /T, co kończy również drzewo własnego daemona. Taki test nie dowodzi samodzielnego sprzątania osieroconych agentów przez recovery. Test Rust crash_daemon zabija tylko rodzica i jest pod tym względem lepszy, lecz test recovery dopuszcza brak znalezionego orphan PID przez warunkowe if let.
- Asercja overlap w skrypcie opiera się na różnicy started_at poniżej 5 s, a nie przecięciu rzeczywistych przedziałów życia. Własne eksperymenty uzupełniły to o przecięcie cykli wykonania zadań.
- Brakuje fault injection na granicach commit/spawn/result; zwykły reopen SQLite nie symuluje całego crash recovery.
- Brakuje regresji BLOCKED po retry, częściowych claims, utraty heartbeat, opóźnionego kill, cancellation pod stdin backpressure i aliasów Windows.
- Nie ma dowodu długiego soak/stress z wieloma generacjami połączeń i prób. Nie stwierdzam flaky testów jedynie na podstawie jednego poprawnego przebiegu.

Minimalny kierunek poprawy testów to nie podbijanie liczby, lecz asercje invariantów: **żywy proces ma właściciela, slot i próbę; zaakceptowany plan ma kompletny DAG; COMPLETED ma wymagany outcome; jeden zasób ma jednego realnego właściciela; klient zna stan połączenia; fragmentacja nie zmienia treści.**

## Platform Compatibility

| Platforma | Wykonano | Ocena |
| --- | --- | --- |
| Windows GNU | Build, fmt, Clippy, 145 testów, E2E 30/30, własne kontrolowane próby | Sprawdzone normalne ścieżki; potwierdzone wady A01–A18 |
| Windows MSVC | NOT RUN | Brak lokalnego dowodu drugiego toolchainu |
| Ubuntu/Linux | NOT RUN natywnie | A19 prawdopodobnie blokuje kompilację; process groups, sygnały, UDS i uprawnienia wymagają rzeczywistego runnera |
| macOS | NOT RUN natywnie | Jak wyżej, dodatkowo różnice CLI/shim, sygnałów i filesystemu |

Unixowy kill_tree ignoruje kody błędów libc::kill i zwraca Ok. Windows ignoruje exit status taskkill i także potrafi raportować sukces mimo nieudanego zakończenia. process_alive uruchamia tasklist i przy błędzie zwraca false. Te uproszczenia należy usunąć podczas napraw lifecycle; same gałęzie cfg nie są dowodem działania.

Minimalna macierz CI:

1. windows-latest: fmt/check, Clippy, workspace tests, release oraz kontrolowane lifecycle/worktree/IPC E2E; objąć GNU używany lokalnie albo osobny job GNU i podstawowy MSVC.
2. ubuntu-latest: te same bramki, UDS permissions/stale endpoint, TERM→KILL, group cleanup, no-orphans, symlink/temp ownership.
3. macos-latest: te same bramki oraz process group/reaping, UDS i ścieżki z Unicode/spacjami.
4. W każdym runnerze limit czasu, prywatny temp, kontrolowane procesy i pełne cleanup. Płatne modele nie są wymagane.
5. Realne CLI adapterów jako osobna, jawnie opcjonalna/bramkowana macierz zgodności; podstawowa niezawodność nie może od nich zależeć.

Nie instalowano brakujących platform ani nie uruchamiano niezweryfikowanego emulowanego środowiska jako zastępstwa testów natywnych.

## Sprint 02 Readiness

### Decyzja: NO-GO

Obecny engine obsłużył 20 prostych zadań. Nie zachowuje jednak invariantów potrzebnych do autonomicznej naprawy, replanningu i powtarzania operacji. Inteligentny orchestrator zwiększy częstość retry/cancel, rozmiar planów i liczbę granic awarii; nie skompensuje braku durable execution identity.

| Funkcja Sprintu 02 | Obecny fundament | Warunek przed autonomią |
| --- | --- | --- |
| Dynamiczne generowanie planów | Istnieje PlanFile + validator | Limity rozmiaru/budżetu, schema version, pełna transakcja przyjęcia |
| Dynamiczne dodawanie zadań | Brak kontraktu mutacji DAG aktywnego run | Wersjonowana operacja z walidacją cykli i stanu zależności |
| Dependencies | Zwykłe gating działa | Naprawić A08 i wiarygodność COMPLETED |
| Wersjonowanie/replanning | Brak osobnej tożsamości plan/run/attempt | Niezmienne wersje i reguły zastępowania zadań, zachowanie pochodzenia |
| Wybór wykonawcy przez model | capabilities i preferred agent dostępne | Validator dopuszczonych agentów; model nie omija slots/policy |
| Zmiana priorytetu | Pole istnieje, brak jawnego kontraktu aktualizacji | Idempotentny command i określona fairness |
| Przeniesienie pracy na innego agenta | Możliwe koncepcyjnie przez nową próbę | Stop/fence starej próby, kontrola bazy i przekazywanych artefaktów |
| Niezależny review | REVIEW_PENDING jest przelotny | Rzeczywisty gate z odrębną decyzją/artefaktem |
| Wspólna wiedza / MCP | Nie są obecnym runtime boundary | Dostęp zależny od run/attempt, pochodzenie danych, ograniczone capabilities |
| Integracja kodu | Brak automatycznej integracji | Actual diff verification, review, CAS target SHA i rollback |
| 10–20 agentów | Normalny kontrolowany workload działa | Naprawić procesy, leases, storage, framing i pamięć; testy pod awariami |

Nie trzeba projektować całego orchestratora od nowa. Potrzebny jest stabilny interfejs **propose plan / validate / create run / observe attempt / cancel / retry / review / integrate**, przy zachowaniu deterministycznego silnika jako właściciela wykonania.

## Prioritized Remediation Plan

### 1. Must fix before Sprint 02

| Kolejność | Zakres | Powiązane ustalenia | Kryterium odbioru |
| --- | --- | --- | --- |
| 1 | Tożsamość i lifecycle procesu, bezpieczne recovery | A01–A03 | Nigdy kill tylko na podstawie starego PID; brak potomków bez właściciela; zapis sesji przed pracą |
| 2 | Wspólna tożsamość attempt i occupancy | A04–A05 | Timeout obejmuje spawn/stdin/drain; cancel nie zwalnia slotu do exit; brak opóźnionych efektów starej próby |
| 3 | Granice transakcji i durable outcome | A06–A07 | Replay zawsze kompletny; brak COMPLETED bez artefaktów i RUNNING bez reconciler/process owner |
| 4 | Claims i leases | A09–A11 | Cały zestaw atomowy; aliases rozpoznane; utrata heartbeat nie dopuszcza dwóch właścicieli |
| 5 | Postęp DAG po retry | A08 | Dziecko uruchamia się raz po sukcesie wszystkich rodziców |
| 6 | Cancellation-safe i bounded I/O | A12–A14 | Fragmentacja nie gubi ramek; większy diff nie blokuje pipe; pamięć nie rośnie z pełną linią |
| 7 | Minimalna weryfikacja scope/review przed integracją | Ograniczenia Git | Immutable base, kontrola rzeczywistego diff i osobna decyzja review |

Zależności: tożsamość attempt z kroku 2 powinna poprzedzać finalizację/replay wyników oraz generacyjne odpowiedzi Git i TUI. Nie trzeba czekać z lokalną naprawą framing/pipe na migrację całego modelu danych; te poprawki można przygotować niezależnie, zachowując audit-only tego raportu.

### 2. Should fix during Sprint 02

- A15: uczciwe liczniki strat, retencja i obserwowalność.
- A16–A17: przepływ OUTPUT i lifecycle transportu TUI; najlepiej jeszcze przed użyciem TUI do nadzorowania eksperymentów orchestratora.
- A18: terminy handshake/write i budżety klientów.
- A19: natywne CI i naprawa importu Unix przed deklaracją multiplatformowości.
- A20: jawna polityka wspieranych executables/batch; do tego czasu tylko kontrolowane native executables.
- A21: prywatny scratch doctor przed szerszym wsparciem Unix.
- Stabilne project_id w ścieżkach, jawna semantyka retry base, poprawne zwracanie rzeczywistego branch.
- Wersje snapshotów i generacje połączeń, limity historii/planów, określona fairness.
- Jeden testowany backend izolacji i oddzielony katalog runtime/credentials od agent-visible workspace.

### 3. Can wait until later

- Dynamiczny plugin ABI adapterów.
- Rozbudowany indeks wspólnej wiedzy i optymalizacja dużej historii, dopóki nie ma realnego workloadu.
- Garbage collection worktrees z polityką retencji i potwierdzeniem własności; obecne zachowanie „zachowaj pracę” jest bezpieczniejszym punktem wyjścia.
- Dalsze strojenie UI i mikrooptymalizacje renderu bez pomiaru problemu.

Proponowane zmiany są rekomendacjami. W audycie nie wdrożono żadnej poprawki i nie zamknięto żadnego ustalenia jako naprawionego.

## Verification Gaps

| Obszar | Status / przyczyna | Bezpieczna alternatywa / dalszy dowód |
| --- | --- | --- |
| Dokładny crash pomiędzy spawn a PID commit | NOT RUN z kontrolowanym zatrzymaniem w konkretnej instrukcji | Source trace i wymuszony błąd sesji A02; potrzebny fault-injection hook |
| Dokładny crash w finalizacji outcome | NOT RUN w konkretnym commit boundary | Triggery zapisu A07; potrzebny test restartu na każdej granicy |
| Fizyczny disk-full / write I/O / power-loss | NOT RUN | Nie zapełniano dysku; izolowane błędy SQL dowodzą nieobsłużonych Result, nie symulują hardware |
| DB worker panic i pełny kanał 1024 jobs | NOT RUN dynamicznie | Inspekcja WorkerGone i lifecycle; wymagany test degraded mode |
| Naturalny Windows PID reuse | NOT RUN | Bezpieczna symulacja starego PID wskazującego własny proces, A01 |
| Unix TERM-ignore, process groups i UDS | NOT RUN natywnie | Analiza cfg, A19, wymagane CI Linux/macOS |
| Junction/symlink/reparse-point matrix | NOT RUN pełnej macierzy | Analiza nieużywanego ensure_inside i ograniczeń normalizacji |
| Rzeczywista operacja poza worktree przeciw danym użytkownika | NOT RUN celowo | Z kodu wynika brak OS sandboxa; nie potrzeba naruszać danych, by to ustalić |
| Dodatkowa interpretacja polecenia przez batch | Demonstracja dodatkowej komendy NOT RUN | Istniejący test korupcji argumentu i przepływ rzeczywistych adapterów A20 |
| Shared-temp doctor na Unix | NOT RUN | Statyczna analiza A21; test tylko z własnym plikiem w przyszłym CI |
| Pełny interaktywny resize terminala / klawiatura podczas stall | NOT RUN pełnego PTY scenariusza | 336 TestBackend renderów i analiza event loop; E2E attach/detach |
| Długi soak 20 rzeczywistych agentów / intensywne JSON warning events | NOT RUN | 10/20 kontrolowanych zadań i ograniczony flood; bez kosztów modeli |
| Realne Codex/OpenCode E2E | NOT RUN w tym audycie | Deklaracje implementatora pozostają historyczne; infrastruktura sprawdzona na atrapach |
| Zależności / CVE / system ACL niestandardowych katalogów | Poza pełną weryfikacją | Osobny ograniczony przegląd; brak twierdzenia o kompletnej ocenie zależności |

Nie ma podstaw do twierdzenia o gotowości produkcyjnej ani do uniwersalnej deklaracji wsparcia wszystkich platform. Jest wystarczająco dużo dowodów, aby wskazać konkretne lokalne naprawy i zachować istniejący podział architektury.

## Stan repozytorium i artefakty

Początkowy git status --short był pusty. Kod produkcyjny, testy repozytorium, Cargo.lock i konfiguracja nie zostały zmodyfikowane. Dodatkowe testy i kontrolne repozytoria istniały wyłącznie w prywatnych katalogach tymczasowych; kompilacje tworzyły standardowe ignorowane pliki target.

Docelowym jedynym nowym trwałym plikiem w repozytorium jest:

- docs/SPRINT_01_INDEPENDENT_AUDIT.md

Końcowe sprawdzenie potwierdziło:

~~~text
git rev-parse HEAD
3f58eecc65a4e1f76e7693732ab842eb6aea5f47

git status --short
?? docs/SPRINT_01_INDEPENDENT_AUDIT.md

git diff --exit-code
[exit 0; brak zmian tracked files]
~~~

Standardowe ignorowane artefakty kompilacji pozostały w target. Nie wykonano commita ani pusha.

Automatyczna kontrola odrzuciła polecenie usunięcia dwóch własnych katalogów tymczasowych z komunikatem „blocked by policy”. Nie ponawiano usuwania innym mechanizmem. Poza repozytorium pozostały dane testowe w katalogach:

- C:\Users\mikio\AppData\Local\Temp\codex-security-artifacts-fab646b0db5a30ddbb85b0012ab40a669fc543eec8d7859fc6710b80f382a15e
- C:\Users\mikio\AppData\Local\Temp\forge-independent-e2e-f0188d34279c4b81bba113565f3f0dba

Wszystkie uruchomienia eksperymentów i skrypt E2E zakończyły się. Katalogi zawierają wyłącznie kontrolowane dane/kopie audytu; ich pozostawienie jest jawnym ograniczeniem sprzątania, nie zmianą aplikacji.

Workflow Codex Security zachował poza repozytorium własne metadane i dowody. Jego dwa ustalenia P2 dotyczą dostępności lokalnego IPC; 21 ustaleń niniejszego raportu obejmuje szerszą niezawodność operacyjną. Pokrycie skanu bezpieczeństwa oznaczono jako częściowe z jawnymi lukami Unix/batch. Ostrzeżenie workflow o zmianie working tree wynika z dodania niniejszego raportu; kontrola powyżej potwierdza niezmieniony HEAD i kod tracked files.

Pomiar użycia zwrócony przez host workflow (nie estymacja kosztu): totalTokens=12 318 696, inputTokens=12 237 591, cachedInputTokens=11 568 128, coverage=complete. To metryka hosta dla trzech wątków, nie liczba nowych tokenów samego raportu.

