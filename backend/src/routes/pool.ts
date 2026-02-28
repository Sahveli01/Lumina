import { Router, Request, Response } from 'express';
import { PoolService } from '../services/poolService';

const router = Router();
const poolService = new PoolService();

// GET /api/pools — List all liquidity pools
router.get('/', async (_req: Request, res: Response) => {
  try {
    const pools = await poolService.listPools();
    res.json({ success: true, data: pools });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// GET /api/pools/:type — Get pool details (senior|junior)
router.get('/:type', async (req: Request, res: Response) => {
  try {
    const pool = await poolService.getPool(req.params.type as 'senior' | 'junior');
    if (!pool) return res.status(404).json({ success: false, error: 'Pool not found' });
    res.json({ success: true, data: pool });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// POST /api/pools/:type/deposit — Deposit liquidity
router.post('/:type/deposit', async (req: Request, res: Response) => {
  try {
    const result = await poolService.deposit(req.params.type as 'senior' | 'junior', req.body);
    res.json({ success: true, data: result });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// POST /api/pools/:type/withdraw — Withdraw liquidity
router.post('/:type/withdraw', async (req: Request, res: Response) => {
  try {
    const result = await poolService.withdraw(req.params.type as 'senior' | 'junior', req.body);
    res.json({ success: true, data: result });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export default router;
