import { createRoot } from 'react-dom/client';
import App from './App';
import './styles/tokens.css';
import './styles/app.css';

const root = document.getElementById('root');
if (!root) throw new Error('#root 不存在');

// 不使用 StrictMode：开发模式下的双重副作用会重复触发 bootstrap / 事件订阅。
createRoot(root).render(<App />);
