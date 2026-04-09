pub struct HttpRequestHandler {
    baseUrl: String,
    maxRetries: u32,
}

impl HttpRequestHandler {
    pub fn handleRequest(&self) -> Result<(), Error> {
        todo!()
    }

    pub fn getBaseUrl(&self) -> &str {
        &self.baseUrl
    }
}
