import { nextTestSetup } from 'e2e-utils'

describe('Skipped in Turbopack', () => {
  describe('app-dir - optimizePackageImports - basic', () => {
    const { next } = nextTestSetup({
      files: __dirname,
    })

    it('should build successfully', async () => {
      // Ensure that MUI is working
      const $ = await next.render$('/')
      expect(await $('#client-mod').text()).toContain('client:default')
    })
  })
})
