import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  getDiagnostics,
  getPricing,
  getSummary,
  getThreads,
  exportCsvFiles,
  refreshImport,
} from "./api";
import {
  fmtInt,
  fmtMoney,
  fmtPct,
  fmtTokens,
  type DiagnosticsDto,
  type PricingInfoDto,
  type SummaryDto,
  type ThreadRow,
} from "./types";

type Tab = "dashboard" | "threads" | "diagnostics";

const NOW = new Date();
const iso = (d: Date) => d.toISOString().slice(0, 10);
const DEFAULT_FROM = iso(new Date(NOW.getTime() - 6 * 86400_000));
const DEFAULT_TO = iso(NOW);

function Card(props: { label: string; value: string; sub?: string; cls?: string }) {
  return (
    <div className={`card ${props.cls ?? ""}`}>
      <div className="label">{props.label}</div>
      <div className="value">{props.value}</div>
      {props.sub && <div className="sub">{props.sub}</div>}
    </div>
  );
}

function DailyChart({ byDay }: { byDay: SummaryDto["by_day"] }) {
  const max = Math.max(1, ...byDay.map((b) => b.input));
  return (
    <div className="chart">
      {byDay.map((b) => (
        <div className="bar-wrap" key={b.name} title={`${b.name} — ${fmtTokens(b.input)}`}>
          <div className="bar" style={{ height: `${(b.input / max) * 100}%` }} />
          <div className="bar-label">{b.name.slice(5)}</div>
        </div>
      ))}
    </div>
  );
}

function BucketTable({ rows, labelHeader }: { rows: SummaryDto["by_model"]; labelHeader: string }) {
  return (
    <table>
      <thead>
        <tr>
          <th>{labelHeader}</th>
          <th className="num">Input</th>
          <th className="num">Cached</th>
          <th className="num">Output</th>
          <th className="num">Cache hit</th>
          <th className="num">Appels</th>
          <th className="num">Équivalent</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((b) => (
          <tr key={b.name}>
            <td>{b.name}</td>
            <td className="num">{fmtTokens(b.input)}</td>
            <td className="num">{fmtTokens(b.cached)}</td>
            <td className="num">{fmtTokens(b.output)}</td>
            <td className="num">{fmtPct(b.cache_hit_percent)}</td>
            <td className="num">{fmtInt(b.calls)}</td>
            <td className="num">{b.unknown_model_tokens > 0 ? "N/A" : fmtMoney(b.equivalent_cost)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

export default function App() {
  const [tab, setTab] = useState<Tab>("dashboard");
  const [from, setFrom] = useState(DEFAULT_FROM);
  const [to, setTo] = useState(DEFAULT_TO);
  const [summary, setSummary] = useState<SummaryDto | null>(null);
  const [threads, setThreads] = useState<ThreadRow[]>([]);
  const [diag, setDiag] = useState<DiagnosticsDto | null>(null);
  const [pricing, setPricing] = useState<PricingInfoDto[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const loadSummary = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setSummary(await getSummary(from || null, to || null));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }, [from, to]);

  const loadThreads = useCallback(async () => {
    setBusy(true);
    try {
      setThreads(await getThreads(from || null, to || null, 40));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }, [from, to]);

  const loadDiagnostics = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setDiag(await getDiagnostics());
      setPricing(await getPricing());
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    if (tab === "dashboard") void loadSummary();
    else if (tab === "threads") void loadThreads();
    else void loadDiagnostics();
  }, [tab, loadSummary, loadThreads, loadDiagnostics]);

  const onRefresh = async () => {
    setBusy(true);
    try {
      await refreshImport();
      await (tab === "threads" ? loadThreads() : loadSummary());
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const onExport = async () => {
    const dir = await open({ directory: true, multiple: false, title: "Dossier d'export CSV" });
    if (!dir) return;
    try {
      const files = await exportCsvFiles(dir as string, from || null, to || null);
      alert(`Export CSV écrit :\n${files.join("\n")}`);
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="app">
      <h1>Codex Usage Monitor</h1>
      <div className="muted">
        Usage observé depuis les logs locaux — « équivalent » = valorisation grille Codex, jamais une facture OpenAI.
      </div>

      <div className="toolbar">
        <label>
          Du&nbsp;
          <input type="date" value={from} onChange={(e) => setFrom(e.target.value)} />
        </label>
        <label>
          au&nbsp;
          <input type="date" value={to} onChange={(e) => setTo(e.target.value)} />
        </label>
        <button onClick={onRefresh} disabled={busy}>
          {busy ? "…" : "Rafraîchir"}
        </button>
        <button className="secondary" onClick={onExport} disabled={busy}>
          Export CSV
        </button>
      </div>

      <div className="tabbar">
        <button className={tab === "dashboard" ? "active" : ""} onClick={() => setTab("dashboard")}>
          Dashboard
        </button>
        <button className={tab === "threads" ? "active" : ""} onClick={() => setTab("threads")}>
          Threads
        </button>
        <button className={tab === "diagnostics" ? "active" : ""} onClick={() => setTab("diagnostics")}>
          Diagnostics
        </button>
      </div>

      {error && <div className="error-box">{error}</div>}

      {tab === "dashboard" && summary && (
        <>
          <div className="cards">
            <Card label="Input" value={fmtTokens(summary.input)} sub={`${fmtInt(summary.calls)} appels`} />
            <Card label="Cached input" value={fmtTokens(summary.cached)} />
            <Card label="Cache hit" value={fmtPct(summary.cache_hit_percent)} cls="green" />
            <Card label="Output" value={fmtTokens(summary.output)} />
            <Card
              label="Équivalent Codex"
              value={fmtMoney(summary.equivalent_cost)}
              sub={`confiance tarifaire ${summary.pricing_confidence_percent.toFixed(1)} %`}
              cls="accent"
            />
            <Card
              label="Économie cache"
              value={fmtMoney(summary.cache_savings)}
              sub={`sans cache : ${fmtMoney(summary.cost_without_cache)}`}
              cls="green"
            />
          </div>

          {summary.tiers[2] > 0 && (
            <div className="anomaly">
              Service tier non déterminé pour {fmtInt(summary.tiers[2])} appel(s) — valorisés au tarif standard
              (fast/régional non appliqués).
            </div>
          )}

          <div className="grid2">
            <div className="panel">
              <h2>Usage quotidien (input)</h2>
              <DailyChart byDay={summary.by_day} />
            </div>
            <div className="panel">
              <h2>Par modèle</h2>
              <BucketTable rows={summary.by_model} labelHeader="Modèle" />
            </div>
          </div>

          <div className="grid2">
            <div className="panel">
              <h2>Par activité</h2>
              <BucketTable rows={summary.by_activity} labelHeader="Activité" />
            </div>
            <div className="panel">
              <h2>Par projet</h2>
              <BucketTable rows={summary.by_project} labelHeader="Projet" />
            </div>
          </div>

          {summary.anomalies.length > 0 && (
            <div className="panel">
              <h2>Anomalies</h2>
              {summary.anomalies.map((a, i) => (
                <div className="anomaly" key={i}>
                  {a}
                </div>
              ))}
            </div>
          )}
        </>
      )}

      {tab === "threads" && (
        <div className="panel">
          <h2>Threads (top 40 par input)</h2>
          <table>
            <thead>
              <tr>
                <th>Thread</th>
                <th>Projet</th>
                <th>Activité</th>
                <th className="num">Input</th>
                <th className="num">Output</th>
                <th className="num">Cache hit</th>
                <th className="num">Appels</th>
                <th className="num">Équivalent</th>
              </tr>
            </thead>
            <tbody>
              {threads.map((t) => (
                <tr key={t.thread_id}>
                  <td>
                    <span className="thread-title">{t.title || "(sans titre)"}</span>
                    <span className="thread-id">{t.thread_id}</span>
                  </td>
                  <td>{t.project}</td>
                  <td>
                    <span className={`badge ${t.activity}`}>{t.activity}</span>
                  </td>
                  <td className="num">{fmtTokens(t.input)}</td>
                  <td className="num">{fmtTokens(t.output)}</td>
                  <td className="num">{fmtPct(t.cache_hit_percent)}</td>
                  <td className="num">{fmtInt(t.calls)}</td>
                  <td className="num">{fmtMoney(t.equivalent_cost)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {tab === "diagnostics" && diag && (
        <>
          <div className="cards">
            <Card label="Rollout files" value={fmtInt(diag.files_found)} sub={`${fmtInt(diag.files_parsed)} parsés`} />
            <Card label="Usage records" value={fmtInt(diag.usage_records)} />
            <Card label="Doublons ignorés" value={fmtInt(diag.duplicates_ignored)} />
            <Card label="Legacy records" value={fmtInt(diag.legacy_records)} />
            <Card
              label="Cohérence"
              value={diag.consistent ? "CONSISTENT" : "DIVERGENCE"}
              cls={diag.consistent ? "green" : ""}
              sub={`base ${fmtInt(diag.db_calls)} appels vs scan frais`}
            />
          </div>

          <div className="grid2">
            <div className="panel">
              <h2>Sources</h2>
              <table>
                <tbody>
                  <tr><td>Codex home</td><td>{diag.codex_home}</td></tr>
                  <tr><td>Base SQLite</td><td>{diag.db_path}</td></tr>
                  <tr><td>Lignes scannées</td><td className="num">{fmtInt(diag.lines_total)}</td></tr>
                  <tr><td>Erreurs de parsing</td><td className="num">{fmtInt(diag.parse_errors.length)}</td></tr>
                  <tr><td>Violations de sous-ensembles</td><td className="num">{fmtInt(diag.subset_violations)}</td></tr>
                  <tr>
                    <td>state DB lifetime tokens_used</td>
                    <td className="num">{fmtTokens(diag.state_lifetime_tokens_used)} <span className="muted">(indication seule)</span></td>
                  </tr>
                </tbody>
              </table>
              {diag.parse_errors.length > 0 && (
                <div className="anomaly" style={{ marginTop: 10 }}>
                  {diag.parse_errors.slice(0, 5).map((e, i) => (
                    <div key={i}>{e}</div>
                  ))}
                </div>
              )}
            </div>
            <div className="panel">
              <h2>Threads state db</h2>
              <table>
                <thead>
                  <tr><th>Activité</th><th className="num">Threads</th></tr>
                </thead>
                <tbody>
                  {Object.entries(diag.state_threads_by_activity).map(([k, v]) => (
                    <tr key={k}>
                      <td><span className={`badge ${k}`}>{k}</span></td>
                      <td className="num">{fmtInt(v)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          <div className="panel">
            <h2>Grilles tarifaires</h2>
            <table>
              <thead>
                <tr>
                  <th>Catalogue</th>
                  <th>Profil</th>
                  <th>Version</th>
                  <th>Vérifié</th>
                  <th>Modèles</th>
                </tr>
              </thead>
              <tbody>
                {pricing.map((p) => (
                  <tr key={p.catalog}>
                    <td>{p.catalog}</td>
                    <td>{p.profile}</td>
                    <td>{p.version}</td>
                    <td>{p.last_verified}</td>
                    <td>{p.models.join(", ")}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="muted">
              Coûts = estimations « équivalent » selon la grille versionnée. Aucune donnée ne quitte la machine.
            </p>
          </div>
        </>
      )}
    </div>
  );
}
