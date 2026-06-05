import { NavLink, Navigate, Route, Routes, useLocation } from 'react-router-dom';
import Dashboard from './pages/Dashboard';
import Connectors from './pages/Connectors';
import Tenants from './pages/Tenants';
import Synthesis from './pages/Synthesis';
import Memory from './pages/Memory';
import Audit from './pages/Audit';
import Settings from './pages/Settings';
import FirstRunWizard from './pages/FirstRunWizard';
import { connectorsApi } from './api';
import { useAsync } from './hooks/useAsync';
import { isFirstRunDismissed } from './lib/firstRun';

const NAV: { to: string; label: string }[] = [
  { to: '/dashboard', label: 'Dashboard' },
  { to: '/connectors', label: 'Connectors' },
  { to: '/tenants', label: 'Tenants' },
  { to: '/synthesis', label: 'Synthesis' },
  { to: '/memory', label: 'Memory' },
  { to: '/audit', label: 'Audit log' },
  { to: '/settings', label: 'Settings' },
];

export default function App() {
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">◆</span>
          <span className="brand-name">Knowledge Admin</span>
        </div>
        <nav>
          {NAV.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              className={({ isActive }) =>
                isActive ? 'nav-link nav-link-active' : 'nav-link'
              }
            >
              {item.label}
            </NavLink>
          ))}
        </nav>
      </aside>
      <main className="content">
        <FirstRunGate />
        <Routes>
          <Route path="/" element={<Navigate to="/dashboard" replace />} />
          <Route path="/dashboard" element={<Dashboard />} />
          <Route path="/welcome" element={<FirstRunWizard />} />
          <Route path="/connectors" element={<Connectors />} />
          <Route path="/tenants" element={<Tenants />} />
          <Route path="/synthesis" element={<Synthesis />} />
          <Route path="/memory" element={<Memory />} />
          <Route path="/audit" element={<Audit />} />
          <Route path="/settings" element={<Settings />} />
          <Route path="*" element={<Navigate to="/dashboard" replace />} />
        </Routes>
      </main>
    </div>
  );
}

/**
 * Sends a brand-new operator to the first-run wizard. We only redirect
 * when we positively know the deployment has zero connectors and the
 * wizard has not already been dismissed — and only from the landing
 * routes (`/` or `/dashboard`), so navigating elsewhere is never
 * hijacked. A failed connector list (e.g. auth not configured) leaves
 * `data` undefined and never triggers a redirect.
 */
function FirstRunGate() {
  const location = useLocation();
  const onLanding =
    location.pathname === '/' || location.pathname === '/dashboard';
  // Only probe connectors on the landing routes when the wizard is
  // still pending, so other pages don't pay for an extra request.
  const shouldProbe = onLanding && !isFirstRunDismissed();
  const list = useAsync(
    (signal) =>
      shouldProbe
        ? connectorsApi.listConnectors(signal)
        : Promise.resolve(null),
    [shouldProbe],
  );

  if (shouldProbe && list.data && list.data.length === 0) {
    return <Navigate to="/welcome" replace />;
  }
  return null;
}
