import { NavLink, Navigate, Route, Routes } from 'react-router-dom';
import Dashboard from './pages/Dashboard';
import Connectors from './pages/Connectors';
import Tenants from './pages/Tenants';
import Synthesis from './pages/Synthesis';
import Memory from './pages/Memory';
import Audit from './pages/Audit';
import Settings from './pages/Settings';

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
        <Routes>
          <Route path="/" element={<Navigate to="/dashboard" replace />} />
          <Route path="/dashboard" element={<Dashboard />} />
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
