#include "MainWindow.h"
#include "./ui_mainwindow.h"
#include <QPixmap>
#include <QKeyEvent>

MainWindow::MainWindow(QWidget *parent, ImageController *controller)
    : QMainWindow(parent)
    , ui(new Ui::MainWindow)
    , imageLabel(new QLabel(this))
    , imageController(controller)  // Initialize controller
{
    ui->setupUi(this);

    // Display the first image
    QPixmap pixmap(QString::fromStdString(imageController->getCurrentFile()));

    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(1600, 1200, Qt::KeepAspectRatio));
        imageLabel->setAlignment(Qt::AlignCenter);
        setCentralWidget(imageLabel);
    } else {
        imageLabel->setText("Failed to load image");
        imageLabel->setAlignment(Qt::AlignCenter);
        setCentralWidget(imageLabel);
    }
}

MainWindow::~MainWindow() {
    delete ui;
}

void MainWindow::keyPressEvent(QKeyEvent *event) {
    switch (event->key()) {
    case Qt::Key_Left:
        imageController->previousImage();
        break;
    case Qt::Key_Right:
        imageController->nextImage();
        break;
    default:
        // QMainWindow::keyPressEvent(event);
        break;
    }

    // Update the displayed image
    QPixmap pixmap(QString::fromStdString(imageController->getCurrentFile()));
    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(1600, 1200, Qt::KeepAspectRatio));
    } else {
        imageLabel->setText("Failed to load image");
    }
}
