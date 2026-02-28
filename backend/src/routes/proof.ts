import { Router, Request, Response } from 'express';
import { ProofService } from '../services/proofService';

const router = Router();
const proofService = new ProofService();

// POST /api/proofs/generate — Generate a ZK proof for an invoice
router.post('/generate', async (req: Request, res: Response) => {
  try {
    const proof = await proofService.generateProof(req.body);
    res.json({ success: true, data: proof });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// POST /api/proofs/verify — Verify a ZK proof
router.post('/verify', async (req: Request, res: Response) => {
  try {
    const valid = await proofService.verifyProof(req.body);
    res.json({ success: true, data: { valid } });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// GET /api/proofs/:nullifier — Check nullifier status (double-spend prevention)
router.get('/:nullifier', async (req: Request, res: Response) => {
  try {
    const status = await proofService.checkNullifier(req.params['nullifier'] as string);
    res.json({ success: true, data: status });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export default router;
