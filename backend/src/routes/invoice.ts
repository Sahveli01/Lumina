import { Router, Request, Response } from 'express';
import { InvoiceService } from '../services/invoiceService';

const router = Router();
const invoiceService = new InvoiceService();

// GET /api/invoices — List all invoices for a company
router.get('/', async (req: Request, res: Response) => {
  try {
    const { companyId, status } = req.query;
    const invoices = await invoiceService.listInvoices(
      companyId as string | undefined,
      status as string | undefined
    );
    res.json({ success: true, data: invoices });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// POST /api/invoices — Submit a new invoice for factoring
router.post('/', async (req: Request, res: Response) => {
  try {
    const invoice = await invoiceService.submitInvoice(req.body);
    res.status(201).json({ success: true, data: invoice });
  } catch (err) {
    res.status(400).json({ success: false, error: (err as Error).message });
  }
});

// GET /api/invoices/:id — Get invoice details
router.get('/:id', async (req: Request, res: Response) => {
  try {
    const invoice = await invoiceService.getInvoice(req.params['id'] as string);
    if (!invoice) return res.status(404).json({ success: false, error: 'Invoice not found' });
    res.json({ success: true, data: invoice });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// POST /api/invoices/:id/factor — Factor an invoice (generate ZK proof + submit to contract)
router.post('/:id/factor', async (req: Request, res: Response) => {
  try {
    const result = await invoiceService.factorInvoice(req.params['id'] as string, req.body);
    res.json({ success: true, data: result });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export default router;
