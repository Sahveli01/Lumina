/**
 * Express application factory for the Lumina API.
 *
 * Exported as a plain Application so tests and index.ts can import it without
 * starting the HTTP server.  All middleware, routes, and the global error
 * handler are wired here.
 */

import express, { Application, NextFunction, Request, Response } from 'express';
import cors from 'cors';
import healthRoutes     from './routes/health.routes';
import invoiceRoutes, { registryRouter } from './routes/invoice.routes';
import poolRoutes       from './routes/pool.routes';
import datasourceRoutes from './routes/datasource.routes';
import paymentRoutes    from './routes/payment.routes';

const app: Application = express();

// ── Middleware ────────────────────────────────────────────────────────────────

app.use(cors());
app.use(express.json());

// ── Health check ──────────────────────────────────────────────────────────────

app.use('/health', healthRoutes);

// ── API Routes ────────────────────────────────────────────────────────────────

app.use('/api/invoice', invoiceRoutes);
app.use('/api/pool', poolRoutes);
app.use('/api/registry', registryRouter);
app.use('/api/datasource', datasourceRoutes);
app.use('/api/payment', paymentRoutes);

// ── Global error handler ──────────────────────────────────────────────────────
// Must be the last `app.use` call and must have exactly 4 arguments so
// Express recognises it as an error-handling middleware.

// eslint-disable-next-line @typescript-eslint/no-unused-vars
app.use((err: Error, _req: Request, res: Response, _next: NextFunction) => {
  console.error('[Lumina API] Unhandled error:', err);
  res.status(500).json({
    success: false,
    error: err.message ?? 'Internal server error',
  });
});

export default app;
