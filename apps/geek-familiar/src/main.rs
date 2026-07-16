use geek_familiar::PetApp;
use platform::PlatformBackend;

fn main() {
    let app = PetApp::demo();

    #[cfg(feature = "gtk")]
    {
        let mut backend = platform::gtk::GtkBackend::new();
        backend.run(Box::new(app));
    }

    #[cfg(not(feature = "gtk"))]
    {
        let mut backend = platform::HeadlessBackend::default();
        backend.run(Box::new(app));
    }
}
