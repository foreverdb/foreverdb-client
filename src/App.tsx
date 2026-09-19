import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import "./App.css";

type ClientStatus = {
  id: string;
  label: string;
  product: string;
  version: string | null;
  file_path: string | null;
  file_exists: boolean;
  file_has_data: boolean;
  from_backup: boolean;
  running: boolean;
  custom: boolean;
  addon_version: string | null;
  addon_update: string | null;
  saved_at: number | null;
  running_since: number | null;
  crash_at: number | null;
};

const UNSAVED_WARNING_HOURS = 2;

function clock(unixSeconds: number) {
  return new Date(unixSeconds * 1000).toLocaleTimeString("de-DE", { hour: "2-digit", minute: "2-digit" });
}

/** Hours the running game has gone without writing its SavedVariables. */
function unsavedHours(client: ClientStatus) {
  if (client.running_since === null) return 0;
  const since = Math.max(client.running_since, client.saved_at ?? 0);
  return (Date.now() / 1000 - since) / 3600;
}
type AddonRelease = { version: string; name: string; notes: string; published_at: string; html_url: string; asset_name: string; asset_size: number };
type InstallResult = { version: string; addon_dir: string; running: boolean };
type ClientCandidate = { id: string; label: string; product: string; version: string | null; has_data: boolean };
type Installation = { wow_dir: string | null; installations: string[]; searched: string[]; clients: ClientStatus[] };
type UploadResult = { import_id: string | null; file_path: string; running: boolean; deleted: boolean };
type Settings = { auto_upload: boolean; extra_clients: string[]; extra_installations: string[]; has_github_token: boolean; repository: string };
type Activity = { client: string; kind: "pending" | "uploaded" | "error" | "update"; message: string; result: UploadResult | null };
type LogEntry = Activity & { at: string; id: number };

const REFRESH_INTERVAL_MS = 5000;
const LOG_LIMIT = 8;

function timestamp() {
  return new Date().toLocaleTimeString("de-DE", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function App() {
  const [installation, setInstallation] = useState<Installation | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [result, setResult] = useState<UploadResult | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [log, setLog] = useState<LogEntry[]>([]);
  const [dialog, setDialog] = useState<{ candidates: ClientCandidate[]; error: string } | null>(null);
  const [release, setRelease] = useState<AddonRelease | null>(null);
  const [releaseError, setReleaseError] = useState("");
  const [installing, setInstalling] = useState(false);
  const [installed, setInstalled] = useState<InstallResult | null>(null);

  const refresh = useCallback(async () => {
    try {
      const found = await invoke<Installation>("detect_installation");
      setInstallation(found);
      setSelected((current) => {
        if (current && found.clients.some((client) => client.id === current)) return current;
        return found.clients[0]?.id ?? null;
      });
    } catch (message) {
      setError(String(message));
    }
  }, []);

  // Poll so the file and "game running" status follow what happens in WoW.
  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), REFRESH_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings).catch((message) => setError(String(message)));
  }, []);

  const checkRelease = useCallback(async (force: boolean) => {
    try {
      setRelease(await invoke<AddonRelease>("check_addon_update", { force }));
      setReleaseError("");
    } catch (message) {
      setRelease(null);
      setReleaseError(String(message));
    }
    await refresh();
  }, [refresh]);

  useEffect(() => {
    void checkRelease(false);
  }, [checkRelease]);

  async function installAddon() {
    if (!client) return;
    setInstalling(true);
    setError("");
    setInstalled(null);
    try {
      setInstalled(await invoke<InstallResult>("install_addon", { client: client.id }));
      await refresh();
    } catch (message) {
      setError(String(message));
    } finally {
      setInstalling(false);
    }
  }

  // The Rust watcher reports what it does (game closed, upload started, result).
  useEffect(() => {
    let counter = 0;
    const unlisten = listen<Activity>("activity", (event) => {
      counter += 1;
      const entry: LogEntry = { ...event.payload, at: timestamp(), id: counter };
      setLog((entries) => [entry, ...entries].slice(0, LOG_LIMIT));
      if (event.payload.result) setResult(event.payload.result);
      if (event.payload.kind === "update") void checkRelease(false);
      void refresh();
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [refresh, checkRelease]);

  async function openAddDialog() {
    try {
      setDialog({ candidates: await invoke<ClientCandidate[]>("list_client_candidates"), error: "" });
    } catch (message) {
      setError(String(message));
    }
  }

  async function addClient(dir: string) {
    try {
      setSettings(await invoke<Settings>("add_client", { dir }));
      setDialog(null);
      setSelected(dir);
      await refresh();
    } catch (message) {
      setDialog((current) => (current ? { ...current, error: String(message) } : current));
    }
  }

  async function chooseFolder() {
    const picked = await open({ directory: true, multiple: false, title: "WoW-Installation („World of Warcraft“) oder Client-Ordner (z. B. _classic_era_) wählen" });
    if (typeof picked === "string") await addClient(picked);
  }

  async function removeClient(id: string) {
    try {
      setSettings(await invoke<Settings>("remove_client", { id }));
      if (selected === id) setSelected(null);
      await refresh();
    } catch (message) {
      setError(String(message));
    }
  }

  async function toggleAutoUpload() {
    if (!settings) return;
    try {
      setSettings(await invoke<Settings>("set_auto_upload", { enabled: !settings.auto_upload }));
    } catch (message) {
      setError(String(message));
    }
  }

  const client = installation?.clients.find((entry) => entry.id === selected) ?? null;

  async function upload() {
    if (!client?.file_has_data) return;
    setBusy(true);
    setError("");
    setResult(null);
    try {
      setResult(await invoke<UploadResult>("upload", { client: client.id }));
      await refresh();
    } catch (message) {
      setError(String(message));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="shell">
      <header className="masthead">
        <div className="mark">F<span>DB</span></div>
        <div>
          <p className="eyebrow">ForeverCollect / Desktop uploader</p>
          <h1>Gesammelte Daten hochladen.</h1>
        </div>
      </header>

      <section className="intro">
        <p>Wähle den WoW-Client. Jedes Mal, wenn WoW die ForeverCollect-Datei schreibt (Ausloggen, /reload, Beenden), wird sie automatisch hochgeladen; von jedem Upload bleibt eine Kopie im Archiv.</p>
        <label className="switch">
          <input type="checkbox" checked={settings?.auto_upload ?? true} onChange={toggleAutoUpload} disabled={settings === null} />
          <span className="track" />
          <span>Automatisch hochladen, sobald WoW die Datei schreibt</span>
        </label>
      </section>

      {installation === null && <div className="card"><p className="muted">Suche WoW-Installation ...</p></div>}

      {installation && installation.wow_dir === null && (
        <div className="notice error">
          <strong>Keine World-of-Warcraft-Installation gefunden</strong>
          <span>Gesucht wurde in:</span>
          <ul className="paths">{installation.searched.map((path) => <li key={path}>{path}</li>)}</ul>
          <small>Setze FOREVERDB_WOW_DIR oder füge die Installation hinzu.</small>
          <div className="dialog-actions"><button className="secondary" onClick={openAddDialog}>+ Installation hinzufügen</button></div>
        </div>
      )}

      {installation?.wow_dir && (
        <section className="card">
          <div className="section-heading">
            <div>
              <h2>Client</h2>
              {installation.installations.map((dir) => <p key={dir}>{dir}</p>)}
            </div>
            <button className="link" onClick={openAddDialog}>+ Client hinzufügen</button>
          </div>
          {installation.clients.length === 0 && <p className="muted">Kein installierter Client gefunden. Füge einen Client-Ordner hinzu.</p>}
          <div className="client-tabs" role="tablist" aria-label="WoW-Client">
            {installation.clients.map((entry) => (
              <button
                key={entry.id}
                role="tab"
                aria-selected={entry.id === selected}
                className={entry.id === selected ? "active" : ""}
                onClick={() => { setSelected(entry.id); setResult(null); setError(""); }}
              >
                <strong>{entry.label}</strong>
                <span>{entry.version ?? "Version unbekannt"}</span>
              </button>
            ))}
          </div>
          {client?.custom && (
            <p className="muted small-note">
              Manuell hinzugefügt · <button className="link" onClick={() => removeClient(client.id)}>entfernen</button>
            </p>
          )}

          {client && (
            <div className={`file-status ${client.file_has_data ? "ok" : "missing"}`}>
              <span className="status-dot" />
              <div>
                <strong>{client.file_has_data ? (client.from_backup ? "Sicherungskopie mit Daten bereit" : "Datei bereit") : client.file_exists ? "Noch keine neuen Daten seit dem letzten Upload" : "Noch keine Daten"}</strong>
                <small>{client.file_path ?? "Kein Account-Verzeichnis gefunden"}</small>
                {client.running && <small className="warning">WoW läuft – neue Daten kommen beim Ausloggen oder per /reload.</small>}
                {client.running && unsavedHours(client) >= UNSAVED_WARNING_HOURS && (
                  <small className="alert">
                    Seit {Math.floor(unsavedHours(client))} h nichts gespeichert – ein Absturz würde alles seit dem letzten Speichern verlieren. Im Spiel <code>/fc save</code> eingeben.
                  </small>
                )}
                {client.crash_at !== null && (
                  <small className="alert">
                    WoW ist um {clock(client.crash_at)} abgestürzt{client.saved_at !== null ? `, zuletzt gespeichert ${clock(client.saved_at)}` : ""} – die Daten dieser Sitzung wurden nicht mehr geschrieben.
                  </small>
                )}
              </div>
            </div>
          )}

          {client && (
            <div className={`addon-status ${client.addon_update ? "update" : client.addon_version ? "ok" : "missing"}`}>
              <div>
                <strong>
                  {client.addon_update
                    ? `ForeverCollect v${client.addon_update} verfügbar`
                    : client.addon_version
                      ? `ForeverCollect v${client.addon_version} installiert`
                      : "ForeverCollect ist nicht installiert"}
                </strong>
                <small>
                  {client.addon_update && client.addon_version && `Installiert: v${client.addon_version} · `}
                  {release && (
                    <>
                      Release vom {new Date(release.published_at).toLocaleDateString("de-DE")} ·{" "}
                      <button className="link" onClick={() => void openUrl(release.html_url)}>Release-Notes</button>
                    </>
                  )}
                  {!release && releaseError && <span className="warning">{releaseError}</span>}
                  {!release && !releaseError && settings && !settings.has_github_token && "Kein GitHub-Token hinterlegt – Update-Prüfung deaktiviert."}
                </small>
                {installed && installed.running && <small className="warning">Installiert – wirkt nach dem nächsten Einloggen bzw. /reload.</small>}
              </div>
              {(client.addon_update || (!client.addon_version && release)) && (
                <button className="secondary" onClick={installAddon} disabled={installing}>
                  {installing ? "Installiere ..." : client.addon_version ? "Aktualisieren" : "Installieren"}
                </button>
              )}
            </div>
          )}

          <button className="primary" onClick={upload} disabled={busy || !client?.file_has_data}>
            {busy ? "Upload läuft ..." : "Jetzt hochladen"}<span>→</span>
          </button>
        </section>
      )}

      {log.length > 0 && (
        <section className="card activity">
          <div className="section-heading"><div><h2>Aktivität</h2><p>Automatische Uploads dieser Sitzung.</p></div></div>
          <ul>
            {log.map((entry) => (
              <li key={entry.id} className={entry.kind}>
                <span className="time">{entry.at}</span>
                <span>{entry.message}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {error && <div className="notice error"><strong>Upload nicht möglich</strong><span>{error}</span></div>}
      {result && (
        <div className="notice success">
          <strong>Upload erfolgreich</strong>
          <span>{result.import_id ? `Import-ID: ${result.import_id}` : "Der Server hat den Upload angenommen."}</span>
          <small>{result.deleted ? "Datei wurde entfernt, die nächste Sitzung startet leer." : "WoW läuft noch: Die Datei bleibt erhalten und wird beim nächsten Schreiben erneut geprüft."}</small>
        </div>
      )}
      {dialog && (
        <div className="backdrop" onClick={() => setDialog(null)}>
          <div className="dialog" role="dialog" aria-modal="true" onClick={(event) => event.stopPropagation()}>
            <div className="section-heading">
              <div>
                <h2>Client hinzufügen</h2>
                <p>Weitere Clients der gefundenen Installationen – oder per Ordnerwahl eine ganze WoW-Installation („World of Warcraft“) bzw. ein einzelner Client-Ordner.</p>
              </div>
            </div>
            {dialog.candidates.length === 0 && <p className="muted">Keine weiteren Client-Ordner in den bekannten Installationen gefunden.</p>}
            <ul className="candidates">
              {dialog.candidates.map((candidate) => (
                <li key={candidate.id}>
                  <div>
                    <strong>{candidate.label}</strong>
                    <span>{candidate.version ?? "nicht im Launcher installiert"}{candidate.has_data ? " · Daten vorhanden" : ""}</span>
                    <small>{candidate.id}</small>
                  </div>
                  <button className="secondary" onClick={() => addClient(candidate.id)}>Hinzufügen</button>
                </li>
              ))}
            </ul>
            {dialog.error && <div className="notice error"><span>{dialog.error}</span></div>}
            <div className="dialog-actions">
              <button className="secondary" onClick={chooseFolder}>Installation oder Ordner wählen …</button>
              <button className="link" onClick={() => setDialog(null)}>Schließen</button>
            </div>
          </div>
        </div>
      )}
      <footer>ForeverDB Client <span>·</span> {settings?.auto_upload ? "Überwachung aktiv" : "Überwachung aus"} <span>·</span> <button className="link" onClick={() => void checkRelease(true)}>Nach Addon-Updates suchen</button></footer>
    </main>
  );
}

export default App;
